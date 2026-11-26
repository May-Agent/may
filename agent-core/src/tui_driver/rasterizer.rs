//! Low-resolution PNG cell rasterizer for virtual terminal snapshots.
//!
//! Draws box-drawing (`U+2500..=U+257F`), block elements (`U+2580..=U+259F`),
//! and braille (`U+2800..=U+28FF`) with pixel-exact procedural geometry so
//! panes and progress indicators align seamlessly without gaps, and rasterizes
//! ASCII, symbols, and CJK (Chinese, Japanese, Korean) glyphs using system
//! fonts discovered via `fontdb` and shaped/rendered with `swash`.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

use image::{ImageFormat, Rgb, RgbImage};
use swash::FontRef;
use swash::scale::{Render, ScaleContext, Source, StrikeWith, image::Content};
use thiserror::Error;

use super::emulator::{Cell, Color, ScreenSnapshot};

/// Default single-column cell width in pixels (`16px` for double-width CJK).
const DEFAULT_CELL_WIDTH: u32 = 8;

/// Default cell height in pixels (2:1 half-width, 1:1 full-width CJK aspect).
const DEFAULT_CELL_HEIGHT: u32 = 16;

/// Maximum cached glyph bitmaps retained per [`CellRasterizer`] instance.
const MAX_GLYPH_CACHE_ENTRIES: usize = 4_096;

/// Default dark terminal background RGB.
const DEFAULT_BG: [u8; 3] = [24, 24, 27];

/// Default terminal foreground RGB.
const DEFAULT_FG: [u8; 3] = [228, 228, 231];

/// Standard 16-color ANSI palette (0..7 normal, 8..15 bright).
const ANSI_PALETTE: [[u8; 3]; 16] = [
    [24, 24, 27],    // 0 black
    [220, 68, 68],   // 1 red
    [74, 189, 108],  // 2 green
    [224, 175, 72],  // 3 yellow
    [86, 156, 214],  // 4 blue
    [188, 118, 226], // 5 magenta
    [78, 198, 204],  // 6 cyan
    [212, 212, 216], // 7 white
    [113, 113, 122], // 8 bright black
    [248, 113, 113], // 9 bright red
    [110, 231, 147], // 10 bright green
    [250, 204, 107], // 11 bright yellow
    [125, 196, 248], // 12 bright blue
    [216, 160, 247], // 13 bright magenta
    [122, 230, 234], // 14 bright cyan
    [250, 250, 250], // 15 bright white
];

/// Cell pixel dimensions for [`CellRasterizer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellMetrics {
    /// Width of one single-column cell in pixels.
    pub width: u32,
    /// Height of one cell row in pixels.
    pub height: u32,
}

impl Default for CellMetrics {
    fn default() -> Self {
        Self {
            width: DEFAULT_CELL_WIDTH,
            height: DEFAULT_CELL_HEIGHT,
        }
    }
}

/// Errors produced during PNG rasterization.
#[derive(Debug, Error)]
pub enum RasterizeError {
    /// PNG encoding failed.
    #[error("failed to encode terminal screen as PNG: {0}")]
    EncodePng(#[from] image::ImageError),
}

#[derive(Debug, Clone)]
struct LoadedFace {
    data: Arc<Vec<u8>>,
    index: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphCacheKey {
    ch: char,
    pixel_width: u32,
    pixel_height: u32,
    bold: bool,
    italic: bool,
}

#[derive(Debug, Clone)]
struct CachedGlyph {
    width: u32,
    height: u32,
    left: i32,
    top: i32,
    ascent: f32,
    alpha: Vec<u8>,
}

struct FontCatalog {
    db: fontdb::Database,
    preferred_mono: Vec<fontdb::ID>,
    preferred_cjk: Vec<fontdb::ID>,
    all_faces: Vec<fontdb::ID>,
    loaded: Mutex<HashMap<fontdb::ID, Option<LoadedFace>>>,
    glyph_cache: Mutex<HashMap<GlyphCacheKey, Option<CachedGlyph>>>,
}

impl std::fmt::Debug for FontCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontCatalog")
            .field("preferred_mono", &self.preferred_mono.len())
            .field("preferred_cjk", &self.preferred_cjk.len())
            .field("total_faces", &self.all_faces.len())
            .finish_non_exhaustive()
    }
}

impl FontCatalog {
    fn load_system() -> Self {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        Self::from_database(db)
    }

    fn from_database(db: fontdb::Database) -> Self {
        let mut preferred_mono = Vec::new();
        let mut preferred_cjk = Vec::new();
        let mut all_faces = Vec::new();

        // Preferred system monospace families across macOS, Linux, and Windows.
        let mono_names = [
            "Menlo",
            "SF Mono",
            "JetBrains Mono",
            "DejaVu Sans Mono",
            "Liberation Mono",
            "Noto Sans Mono",
            "Ubuntu Mono",
            "Consolas",
            "Courier New",
        ];
        for name in mono_names {
            if let Some(id) = db.query(&fontdb::Query {
                families: &[fontdb::Family::Name(name)],
                ..fontdb::Query::default()
            }) && !preferred_mono.contains(&id)
            {
                preferred_mono.push(id);
            }
        }
        if let Some(id) = db.query(&fontdb::Query {
            families: &[fontdb::Family::Monospace],
            ..fontdb::Query::default()
        }) && !preferred_mono.contains(&id)
        {
            preferred_mono.push(id);
        }

        // Preferred CJK families covering Chinese, Japanese, and Korean.
        let cjk_names = [
            "PingFang SC",
            "PingFang TC",
            "Hiragino Sans",
            "Hiragino Kaku Gothic ProN",
            "Apple SD Gothic Neo",
            "Noto Sans CJK SC",
            "Noto Sans CJK JP",
            "Noto Sans CJK KR",
            "Noto Sans Mono CJK SC",
            "Noto Sans Mono CJK JP",
            "Noto Sans Mono CJK KR",
            "Source Han Sans SC",
            "Source Han Sans JP",
            "Source Han Sans KR",
            "WenQuanYi Zen Hei",
            "WenQuanYi Micro Hei",
            "Arial Unicode MS",
            "STHeiti",
            "Yu Gothic",
            "Malgun Gothic",
            "AppleGothic",
            "NanumGothic",
        ];
        for name in cjk_names {
            if let Some(id) = db.query(&fontdb::Query {
                families: &[fontdb::Family::Name(name)],
                ..fontdb::Query::default()
            }) && !preferred_cjk.contains(&id)
            {
                preferred_cjk.push(id);
            }
        }

        for face in db.faces() {
            if face.monospaced && !preferred_mono.contains(&face.id) {
                preferred_mono.push(face.id);
            }
            all_faces.push(face.id);
        }

        Self {
            db,
            preferred_mono,
            preferred_cjk,
            all_faces,
            loaded: Mutex::new(HashMap::new()),
            glyph_cache: Mutex::new(HashMap::new()),
        }
    }

    fn face_data(&self, id: fontdb::ID) -> Option<LoadedFace> {
        if let Ok(guard) = self.loaded.lock()
            && let Some(cached) = guard.get(&id)
        {
            return cached.clone();
        }
        let loaded = self.db.with_face_data(id, |bytes, index| LoadedFace {
            data: Arc::new(bytes.to_vec()),
            index,
        });
        if let Ok(mut guard) = self.loaded.lock() {
            guard.insert(id, loaded.clone());
        }
        loaded
    }

    fn rasterize_char(&self, key: GlyphCacheKey) -> Option<CachedGlyph> {
        if let Ok(guard) = self.glyph_cache.lock()
            && let Some(cached) = guard.get(&key)
        {
            return cached.clone();
        }

        let glyph = self.rasterize_uncached(key);

        if let Ok(mut guard) = self.glyph_cache.lock() {
            if guard.len() >= MAX_GLYPH_CACHE_ENTRIES {
                guard.clear();
            }
            guard.insert(key, glyph.clone());
        }
        glyph
    }

    fn rasterize_uncached(&self, key: GlyphCacheKey) -> Option<CachedGlyph> {
        let prefer_cjk = is_cjk_or_wide(key.ch);
        let primary_list = if prefer_cjk {
            &self.preferred_cjk
        } else {
            &self.preferred_mono
        };
        let secondary_list = if prefer_cjk {
            &self.preferred_mono
        } else {
            &self.preferred_cjk
        };

        for id in primary_list
            .iter()
            .chain(secondary_list.iter())
            .chain(self.all_faces.iter().take(64))
        {
            let Some(face) = self.face_data(*id) else {
                continue;
            };
            let Some(font_ref) = FontRef::from_index(&face.data, face.index as usize) else {
                continue;
            };
            let glyph_id = font_ref.charmap().map(key.ch);
            if glyph_id == 0 {
                continue;
            }
            if let Some(rendered) = render_swash_glyph(font_ref, glyph_id, key) {
                return Some(rendered);
            }
        }
        None
    }
}

fn render_swash_glyph(font: FontRef<'_>, glyph_id: u16, key: GlyphCacheKey) -> Option<CachedGlyph> {
    let target_size = (key.pixel_height as f32 * 0.82).max(8.0);
    let metrics = font.metrics(&[]).scale(target_size);
    let mut context = ScaleContext::new();
    let mut scaler = context.builder(font).size(target_size).hint(true).build();
    let image = Render::new(&[
        Source::ColorOutline(0),
        Source::ColorBitmap(StrikeWith::BestFit),
        Source::Outline,
    ])
    .render(&mut scaler, glyph_id)?;

    let pixel_count =
        usize::try_from(image.placement.width.saturating_mul(image.placement.height)).ok()?;
    let alpha = match image.content {
        Content::Mask => image.data,
        Content::SubpixelMask => image
            .data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|rgb| {
                u8::try_from((u16::from(rgb[0]) + u16::from(rgb[1]) + u16::from(rgb[2])) / 3)
                    .unwrap_or(255)
            })
            .collect(),
        Content::Color => image
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|rgba| rgba[3])
            .collect(),
    };
    if alpha.len() < pixel_count {
        return None;
    }

    Some(CachedGlyph {
        width: image.placement.width,
        height: image.placement.height,
        left: image.placement.left,
        top: image.placement.top,
        ascent: metrics.ascent,
        alpha,
    })
}

const fn is_cjk_or_wide(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1100..=0x11FF
            | 0x2E80..=0x9FFF
            | 0xAC00..=0xD7AF
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFFEF
            | 0x20000..=0x2FA1F
    )
}

/// Low-resolution terminal screen rasterizer owning its system font catalog.
///
/// Holds no global `static` state; clone cheaply to share the loaded font
/// catalog across multiple `tui observe` calls.
#[derive(Debug, Clone)]
pub struct CellRasterizer {
    catalog: Arc<FontCatalog>,
    metrics: CellMetrics,
}

impl Default for CellRasterizer {
    fn default() -> Self {
        Self::new()
    }
}

impl CellRasterizer {
    /// Creates a rasterizer using system fonts and default `8x16` cell metrics.
    #[must_use]
    pub fn new() -> Self {
        Self::with_metrics(CellMetrics::default())
    }

    /// Creates a rasterizer with custom cell pixel dimensions.
    #[must_use]
    pub fn with_metrics(metrics: CellMetrics) -> Self {
        Self {
            catalog: Arc::new(FontCatalog::load_system()),
            metrics: CellMetrics {
                width: metrics.width.max(4),
                height: metrics.height.max(8),
            },
        }
    }

    /// Creates a rasterizer backed by an empty font database (forcing procedural
    /// primitives and the built-in ASCII fallback bitmap font).
    #[must_use]
    pub fn without_system_fonts() -> Self {
        Self {
            catalog: Arc::new(FontCatalog::from_database(fontdb::Database::new())),
            metrics: CellMetrics::default(),
        }
    }

    /// Returns the configured cell pixel dimensions.
    #[must_use]
    pub const fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// Renders `snapshot` into an [`RgbImage`].
    #[must_use]
    pub fn render_image(&self, snapshot: &ScreenSnapshot) -> RgbImage {
        let cell_w = self.metrics.width;
        let cell_h = self.metrics.height;
        let img_w = u32::from(snapshot.cols).saturating_mul(cell_w).max(1);
        let img_h = u32::from(snapshot.rows).saturating_mul(cell_h).max(1);
        let mut image = RgbImage::from_pixel(img_w, img_h, Rgb(DEFAULT_BG));

        for (row_idx, row_cells) in snapshot.cells.iter().enumerate() {
            let Ok(row_u16) = u16::try_from(row_idx) else {
                break;
            };
            let y0 = u32::from(row_u16).saturating_mul(cell_h);
            for (col_idx, cell) in row_cells.iter().enumerate() {
                if cell.is_continuation() {
                    continue;
                }
                let Ok(col_u16) = u16::try_from(col_idx) else {
                    break;
                };
                let span_cols = u32::from(cell.width.max(1));
                let x0 = u32::from(col_u16).saturating_mul(cell_w);
                let px_w = cell_w.saturating_mul(span_cols);

                let is_cursor = snapshot.modes.cursor_visible
                    && snapshot.cursor.row == row_u16
                    && snapshot.cursor.col == col_u16;

                let (mut fg, mut bg) = resolve_cell_colors(cell);
                if is_cursor {
                    std::mem::swap(&mut fg, &mut bg);
                    if fg == bg {
                        bg = DEFAULT_FG;
                        fg = DEFAULT_BG;
                    }
                }

                fill_rect(&mut image, x0, y0, px_w, cell_h, bg);

                let cell_rect = PixelRect {
                    x: x0,
                    y: y0,
                    w: px_w,
                    h: cell_h,
                };
                if !cell.style.hidden && cell.ch != ' ' {
                    self.draw_glyph(&mut image, cell, cell_rect, fg);
                }

                if cell.style.underline {
                    let uy = y0 + cell_h.saturating_sub(2);
                    fill_rect(&mut image, x0, uy, px_w, 1, fg);
                }
                if cell.style.strikethrough {
                    let sy = y0 + cell_h / 2;
                    fill_rect(&mut image, x0, sy, px_w, 1, fg);
                }
            }
        }

        image
    }

    /// Renders `snapshot` directly into compact PNG bytes.
    ///
    /// # Errors
    /// Returns [`RasterizeError::EncodePng`] if PNG encoding fails.
    pub fn render_png(&self, snapshot: &ScreenSnapshot) -> Result<Vec<u8>, RasterizeError> {
        let image = self.render_image(snapshot);
        let mut cursor = Cursor::new(Vec::new());
        image.write_to(&mut cursor, ImageFormat::Png)?;
        Ok(cursor.into_inner())
    }

    fn draw_glyph(&self, image: &mut RgbImage, cell: &Cell, rect: PixelRect, fg: [u8; 3]) {
        let ch = cell.ch;
        if draw_box_drawing(image, ch, rect.x, rect.y, rect.w, rect.h, fg)
            || draw_block_element(image, ch, rect.x, rect.y, rect.w, rect.h, fg)
            || draw_braille(image, ch, rect.x, rect.y, rect.w, rect.h, fg)
        {
            return;
        }

        let key = GlyphCacheKey {
            ch,
            pixel_width: rect.w,
            pixel_height: rect.h,
            bold: cell.style.bold,
            italic: cell.style.italic,
        };
        if let Some(glyph) = self.catalog.rasterize_char(key) {
            composite_glyph(image, &glyph, rect, fg, cell.style.bold);
            return;
        }

        // Minimal built-in 5x7 ASCII fallback when no system font covers the glyph.
        if ch.is_ascii_graphic() {
            draw_ascii_fallback(image, ch as u8, rect.x, rect.y, rect.w, rect.h, fg);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PixelRect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

fn resolve_cell_colors(cell: &Cell) -> ([u8; 3], [u8; 3]) {
    let mut fg = match cell.style.fg {
        Color::Default => DEFAULT_FG,
        Color::Indexed(idx) => {
            let effective = if cell.style.bold && idx < 8 {
                idx + 8
            } else {
                idx
            };
            ansi256_to_rgb(effective)
        }
        Color::Rgb(r, g, b) => [r, g, b],
    };
    let mut bg = match cell.style.bg {
        Color::Default => DEFAULT_BG,
        Color::Indexed(idx) => ansi256_to_rgb(idx),
        Color::Rgb(r, g, b) => [r, g, b],
    };
    if cell.style.reverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.style.dim {
        fg = blend_rgb(bg, fg, 140);
    }
    (fg, bg)
}

fn ansi256_to_rgb(idx: u8) -> [u8; 3] {
    if idx < 16 {
        return ANSI_PALETTE[usize::from(idx)];
    }
    if idx < 232 {
        let v = idx - 16;
        let r = v / 36;
        let g = (v % 36) / 6;
        let b = v % 6;
        let scale = |c: u8| if c == 0 { 0 } else { 55 + c * 40 };
        return [scale(r), scale(g), scale(b)];
    }
    let gray = 8 + (idx - 232) * 10;
    [gray, gray, gray]
}

fn blend_rgb(bg: [u8; 3], fg: [u8; 3], alpha: u8) -> [u8; 3] {
    let a = u16::from(alpha);
    let inv = 255 - a;
    let mix =
        |f: u8, b: u8| u8::try_from((u16::from(f) * a + u16::from(b) * inv) / 255).unwrap_or(255);
    [mix(fg[0], bg[0]), mix(fg[1], bg[1]), mix(fg[2], bg[2])]
}

fn set_pixel(image: &mut RgbImage, x: u32, y: u32, color: [u8; 3]) {
    if x < image.width() && y < image.height() {
        image.put_pixel(x, y, Rgb(color));
    }
}

fn fill_rect(image: &mut RgbImage, x0: u32, y0: u32, w: u32, h: u32, color: [u8; 3]) {
    let max_x = x0.saturating_add(w).min(image.width());
    let max_y = y0.saturating_add(h).min(image.height());
    for y in y0..max_y {
        for x in x0..max_x {
            image.put_pixel(x, y, Rgb(color));
        }
    }
}

fn composite_glyph(
    image: &mut RgbImage,
    glyph: &CachedGlyph,
    rect: PixelRect,
    fg: [u8; 3],
    bold: bool,
) {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "ascent is clamped to 0..=256 pixel range before rounding to i32"
    )]
    let baseline = glyph.ascent.clamp(0.0, 256.0).round() as i32;
    let baseline_clamped = baseline.clamp(1, i32::try_from(rect.h).unwrap_or(16) - 2);
    let gx0 = i32::try_from(rect.x).unwrap_or(0) + glyph.left;
    let gy0 = i32::try_from(rect.y).unwrap_or(0) + baseline_clamped - glyph.top;
    let max_col_px = i32::try_from(rect.x + rect.w).unwrap_or(i32::MAX);
    let max_row_px = i32::try_from(rect.y + rect.h).unwrap_or(i32::MAX);
    let min_col_px = i32::try_from(rect.x).unwrap_or(0);
    let min_row_px = i32::try_from(rect.y).unwrap_or(0);

    for gy in 0..glyph.height {
        let py = gy0 + i32::try_from(gy).unwrap_or(0);
        if py < min_row_px || py >= max_row_px {
            continue;
        }
        let Ok(dst_row) = u32::try_from(py) else {
            continue;
        };
        if dst_row >= image.height() {
            continue;
        }
        for gx in 0..glyph.width {
            let idx = (gy * glyph.width + gx) as usize;
            let raw_alpha = glyph.alpha.get(idx).copied().unwrap_or(0);
            if raw_alpha == 0 {
                continue;
            }
            let alpha = if bold {
                raw_alpha.saturating_add(raw_alpha / 3)
            } else {
                raw_alpha
            };
            let px = gx0 + i32::try_from(gx).unwrap_or(0);
            if px < min_col_px || px >= max_col_px {
                continue;
            }
            let Ok(dst_col) = u32::try_from(px) else {
                continue;
            };
            if dst_col < image.width() {
                let bg = image.get_pixel(dst_col, dst_row).0;
                image.put_pixel(dst_col, dst_row, Rgb(blend_rgb(bg, fg, alpha)));
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    None,
    Light,
    Heavy,
    Double,
}

fn draw_box_drawing(
    image: &mut RgbImage,
    ch: char,
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
    fg: [u8; 3],
) -> bool {
    let cp = ch as u32;
    if !(0x2500..=0x257F).contains(&cp) {
        return false;
    }

    let cx = x0 + w / 2;
    let cy = y0 + h / 2;

    // Special diagonals.
    if matches!(ch, '╱' | '╲' | '╳') {
        if matches!(ch, '╱' | '╳') {
            for step in 0..h {
                let x = x0 + (w - 1).saturating_sub((step * w) / h);
                set_pixel(image, x, y0 + step, fg);
            }
        }
        if matches!(ch, '╲' | '╳') {
            for step in 0..h {
                let x = x0 + (step * w) / h;
                set_pixel(image, x, y0 + step, fg);
            }
        }
        return true;
    }

    // Dashed horizontal / vertical lines.
    match ch {
        '┄' | '┅' | '┈' | '┉' | '╌' | '╍' => {
            let thick = u32::from(matches!(ch, '┅' | '┉' | '╍')) + 1;
            for dx in 0..w {
                if (dx / 2).is_multiple_of(2) {
                    fill_rect(image, x0 + dx, cy, 1, thick, fg);
                }
            }
            return true;
        }
        '┆' | '┇' | '┊' | '┋' | '╎' | '╏' => {
            let thick = u32::from(matches!(ch, '┇' | '┋' | '╏')) + 1;
            for dy in 0..h {
                if (dy / 2).is_multiple_of(2) {
                    fill_rect(image, cx, y0 + dy, thick, 1, fg);
                }
            }
            return true;
        }
        _ => {}
    }

    let (up, right, down, left) = box_arms(ch);
    draw_arm_up(image, up, cx, y0, cy, fg);
    draw_arm_down(image, down, cx, cy, y0 + h, fg);
    draw_arm_left(image, left, x0, cx, cy, fg);
    draw_arm_right(image, right, cx, x0 + w, cy, fg);

    // Ensure clean center junction pixel for single/heavy lines.
    if matches!(up, Arm::Light)
        || matches!(down, Arm::Light)
        || matches!(left, Arm::Light)
        || matches!(right, Arm::Light)
    {
        set_pixel(image, cx, cy, fg);
    }
    if matches!(up, Arm::Heavy)
        || matches!(down, Arm::Heavy)
        || matches!(left, Arm::Heavy)
        || matches!(right, Arm::Heavy)
    {
        fill_rect(image, cx.saturating_sub(1), cy.saturating_sub(1), 2, 2, fg);
    }
    // Miter double-line corners cleanly.
    if matches!(up, Arm::Double)
        || matches!(down, Arm::Double)
        || matches!(left, Arm::Double)
        || matches!(right, Arm::Double)
    {
        let ox_l = cx.saturating_sub(1);
        let ox_r = cx + 1;
        let oy_t = cy.saturating_sub(1);
        let oy_b = cy + 1;
        if matches!(down, Arm::Double) && matches!(right, Arm::Double) {
            fill_rect(image, ox_l, oy_t, 3, 1, fg);
            fill_rect(image, ox_l, oy_t, 1, 3, fg);
        }
        if matches!(down, Arm::Double) && matches!(left, Arm::Double) {
            fill_rect(image, ox_l, oy_t, 3, 1, fg);
            fill_rect(image, ox_r, oy_t, 1, 3, fg);
        }
        if matches!(up, Arm::Double) && matches!(right, Arm::Double) {
            fill_rect(image, ox_l, oy_b, 3, 1, fg);
            fill_rect(image, ox_l, oy_t, 1, 3, fg);
        }
        if matches!(up, Arm::Double) && matches!(left, Arm::Double) {
            fill_rect(image, ox_l, oy_b, 3, 1, fg);
            fill_rect(image, ox_r, oy_t, 1, 3, fg);
        }
    }

    true
}

fn draw_arm_up(image: &mut RgbImage, arm: Arm, cx: u32, y_top: u32, cy: u32, fg: [u8; 3]) {
    let len = cy.saturating_sub(y_top) + 1;
    match arm {
        Arm::None => {}
        Arm::Light => fill_rect(image, cx, y_top, 1, len, fg),
        Arm::Heavy => fill_rect(image, cx.saturating_sub(1), y_top, 2, len, fg),
        Arm::Double => {
            fill_rect(image, cx.saturating_sub(1), y_top, 1, len, fg);
            fill_rect(image, cx + 1, y_top, 1, len, fg);
        }
    }
}

fn draw_arm_down(image: &mut RgbImage, arm: Arm, cx: u32, cy: u32, y_end: u32, fg: [u8; 3]) {
    let len = y_end.saturating_sub(cy);
    match arm {
        Arm::None => {}
        Arm::Light => fill_rect(image, cx, cy, 1, len, fg),
        Arm::Heavy => fill_rect(image, cx.saturating_sub(1), cy, 2, len, fg),
        Arm::Double => {
            fill_rect(image, cx.saturating_sub(1), cy, 1, len, fg);
            fill_rect(image, cx + 1, cy, 1, len, fg);
        }
    }
}

fn draw_arm_left(image: &mut RgbImage, arm: Arm, x_left: u32, cx: u32, cy: u32, fg: [u8; 3]) {
    let len = cx.saturating_sub(x_left) + 1;
    match arm {
        Arm::None => {}
        Arm::Light => fill_rect(image, x_left, cy, len, 1, fg),
        Arm::Heavy => fill_rect(image, x_left, cy.saturating_sub(1), len, 2, fg),
        Arm::Double => {
            fill_rect(image, x_left, cy.saturating_sub(1), len, 1, fg);
            fill_rect(image, x_left, cy + 1, len, 1, fg);
        }
    }
}

fn draw_arm_right(image: &mut RgbImage, arm: Arm, cx: u32, x_end: u32, cy: u32, fg: [u8; 3]) {
    let len = x_end.saturating_sub(cx);
    match arm {
        Arm::None => {}
        Arm::Light => fill_rect(image, cx, cy, len, 1, fg),
        Arm::Heavy => fill_rect(image, cx, cy.saturating_sub(1), len, 2, fg),
        Arm::Double => {
            fill_rect(image, cx, cy.saturating_sub(1), len, 1, fg);
            fill_rect(image, cx, cy + 1, len, 1, fg);
        }
    }
}

const fn box_arms(ch: char) -> (Arm, Arm, Arm, Arm) {
    use Arm::{Double as D, Heavy as H, Light as L, None as N};
    match ch {
        '─' => (N, L, N, L),
        '━' => (N, H, N, H),
        '│' => (L, N, L, N),
        '┃' => (H, N, H, N),
        '┌' | '╭' => (N, L, L, N),
        '┍' => (N, H, L, N),
        '┎' => (N, L, H, N),
        '┏' => (N, H, H, N),
        '┐' | '╮' => (N, N, L, L),
        '┑' => (N, N, L, H),
        '┒' => (N, N, H, L),
        '┓' => (N, N, H, H),
        '└' | '╰' => (L, L, N, N),
        '┕' => (L, H, N, N),
        '┖' => (H, L, N, N),
        '┗' => (H, H, N, N),
        '┘' | '╯' => (L, N, N, L),
        '┙' => (L, N, N, H),
        '┚' => (H, N, N, L),
        '┛' => (H, N, N, H),
        '├' => (L, L, L, N),
        '┝' => (L, H, L, N),
        '┞' => (H, L, L, N),
        '┟' => (L, L, H, N),
        '┠' => (H, L, H, N),
        '┡' => (H, H, L, N),
        '┢' => (L, H, H, N),
        '┣' => (H, H, H, N),
        '┤' => (L, N, L, L),
        '┥' => (L, N, L, H),
        '┦' => (H, N, L, L),
        '┧' => (L, N, H, L),
        '┨' => (H, N, H, L),
        '┩' => (H, N, L, H),
        '┪' => (L, N, H, H),
        '┫' => (H, N, H, H),
        '┬' => (N, L, L, L),
        '┭' => (N, L, L, H),
        '┮' => (N, H, L, L),
        '┯' => (N, H, L, H),
        '┰' => (N, L, H, L),
        '┱' => (N, L, H, H),
        '┲' => (N, H, H, L),
        '┳' => (N, H, H, H),
        '┴' => (L, L, N, L),
        '┵' => (L, L, N, H),
        '┶' => (L, H, N, L),
        '┷' => (L, H, N, H),
        '┸' => (H, L, N, L),
        '┹' => (H, L, N, H),
        '┺' => (H, H, N, L),
        '┻' => (H, H, N, H),
        '┼' => (L, L, L, L),
        '┽' => (L, L, L, H),
        '┾' => (L, H, L, L),
        '┿' => (L, H, L, H),
        '╀' => (H, L, L, L),
        '╁' => (L, L, H, L),
        '╂' => (H, L, H, L),
        '╃' => (H, L, L, H),
        '╄' => (H, H, L, L),
        '╅' => (L, L, H, H),
        '╆' => (L, H, H, L),
        '╇' => (H, H, L, H),
        '╈' => (L, H, H, H),
        '╉' => (H, L, H, H),
        '╊' => (H, H, H, L),
        '╋' => (H, H, H, H),
        '═' => (N, D, N, D),
        '║' => (D, N, D, N),
        '╒' => (N, D, L, N),
        '╓' => (N, L, D, N),
        '╔' => (N, D, D, N),
        '╕' => (N, N, L, D),
        '╖' => (N, N, D, L),
        '╗' => (N, N, D, D),
        '╘' => (L, D, N, N),
        '╙' => (D, L, N, N),
        '╚' => (D, D, N, N),
        '╛' => (L, N, N, D),
        '╜' => (D, N, N, L),
        '╝' => (D, N, N, D),
        '╞' => (L, D, L, N),
        '╟' => (D, L, D, N),
        '╠' => (D, D, D, N),
        '╡' => (L, N, L, D),
        '╢' => (D, N, D, L),
        '╣' => (D, N, D, D),
        '╤' => (N, D, L, D),
        '╥' => (N, L, D, L),
        '╦' => (N, D, D, D),
        '╧' => (L, D, N, D),
        '╨' => (D, L, N, L),
        '╩' => (D, D, N, D),
        '╪' => (L, D, L, D),
        '╫' => (D, L, D, L),
        '╬' => (D, D, D, D),
        '╴' => (N, N, N, L),
        '╵' => (L, N, N, N),
        '╶' => (N, L, N, N),
        '╷' => (N, N, L, N),
        '╸' => (N, N, N, H),
        '╹' => (H, N, N, N),
        '╺' => (N, H, N, N),
        '╻' => (N, N, H, N),
        '╼' => (N, H, N, L),
        '╽' => (L, N, H, N),
        '╾' => (N, L, N, H),
        '╿' => (H, N, L, N),
        _ => (N, N, N, N),
    }
}

fn draw_block_element(
    image: &mut RgbImage,
    ch: char,
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
    fg: [u8; 3],
) -> bool {
    let cp = ch as u32;
    if !(0x2580..=0x259F).contains(&cp) {
        return false;
    }

    match cp {
        // Upper half block ▀
        0x2580 => fill_rect(image, x0, y0, w, h / 2, fg),
        // Lower 1/8 .. 8/8 blocks ▁..█
        0x2581..=0x2588 => {
            let eighths = cp - 0x2580;
            let bh = (h * eighths).div_ceil(8).min(h);
            fill_rect(image, x0, y0 + h - bh, w, bh, fg);
        }
        // Left 7/8 .. 1/8 blocks ▉..▏
        0x2589..=0x258F => {
            let eighths = 8 - (cp - 0x2588);
            let bw = (w * eighths).div_ceil(8).min(w);
            fill_rect(image, x0, y0, bw, h, fg);
        }
        // Right half block ▐
        0x2590 => fill_rect(image, x0 + w / 2, y0, w - w / 2, h, fg),
        // Light, medium, dark shades ░▒▓
        0x2591..=0x2593 => {
            let alpha = match cp {
                0x2591 => 64,
                0x2592 => 128,
                _ => 192,
            };
            for dy in 0..h {
                for dx in 0..w {
                    let x = x0 + dx;
                    let y = y0 + dy;
                    if x < image.width() && y < image.height() {
                        let bg = image.get_pixel(x, y).0;
                        image.put_pixel(x, y, Rgb(blend_rgb(bg, fg, alpha)));
                    }
                }
            }
        }
        // Upper 1/8 block ▔
        0x2594 => fill_rect(image, x0, y0, w, h.div_ceil(8), fg),
        // Right 1/8 block ▕
        0x2595 => {
            let bw = w.div_ceil(8);
            fill_rect(image, x0 + w - bw, y0, bw, h, fg);
        }
        // Quadrant blocks ▖..▟ (bitmask: 1=UL, 2=UR, 4=LL, 8=LR)
        0x2596..=0x259F => {
            let mask: u8 = match cp {
                0x2596 => 0b0100,
                0x2597 => 0b1000,
                0x2598 => 0b0001,
                0x2599 => 0b1101,
                0x259A => 0b1001,
                0x259B => 0b0111,
                0x259C => 0b1011,
                0x259D => 0b0010,
                0x259E => 0b0110,
                0x259F => 0b1110,
                _ => 0,
            };
            let hw = w / 2;
            let hh = h / 2;
            if (mask & 0b0001) != 0 {
                fill_rect(image, x0, y0, hw, hh, fg);
            }
            if (mask & 0b0010) != 0 {
                fill_rect(image, x0 + hw, y0, w - hw, hh, fg);
            }
            if (mask & 0b0100) != 0 {
                fill_rect(image, x0, y0 + hh, hw, h - hh, fg);
            }
            if (mask & 0b1000) != 0 {
                fill_rect(image, x0 + hw, y0 + hh, w - hw, h - hh, fg);
            }
        }
        _ => return false,
    }
    true
}

fn draw_braille(
    image: &mut RgbImage,
    ch: char,
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
    fg: [u8; 3],
) -> bool {
    // Standard Unicode braille dot layout (bit -> (col 0..2, row 0..4)).
    const DOTS: [(u8, u32, u32); 8] = [
        (0, 0, 0),
        (1, 0, 1),
        (2, 0, 2),
        (6, 0, 3),
        (3, 1, 0),
        (4, 1, 1),
        (5, 1, 2),
        (7, 1, 3),
    ];
    let cp = ch as u32;
    if !(0x2800..=0x28FF).contains(&cp) {
        return false;
    }
    let Ok(bits) = u8::try_from(cp - 0x2800) else {
        return false;
    };
    let dot_w = (w / 2).saturating_sub(1).max(1);
    let dot_h = (h / 4).saturating_sub(1).max(1);
    for (bit, col, row) in DOTS {
        if (bits & (1 << bit)) != 0 {
            let dx = x0 + col * (w / 2) + 1;
            let dy = y0 + row * (h / 4) + 1;
            fill_rect(image, dx, dy, dot_w, dot_h, fg);
        }
    }
    true
}

/// Compact 5x7 bitmap fallback for printable ASCII (`0x21..=0x7E`) used only
/// when no system font is available on the host.
const ASCII_5X7: [[u8; 7]; 94] = [
    [0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x04], // !
    [0x0A, 0x0A, 0x00, 0x00, 0x00, 0x00, 0x00], // "
    [0x0A, 0x1F, 0x0A, 0x0A, 0x1F, 0x0A, 0x00], // #
    [0x04, 0x0F, 0x14, 0x0E, 0x05, 0x1E, 0x04], // $
    [0x19, 0x1A, 0x04, 0x04, 0x0B, 0x13, 0x00], // %
    [0x0C, 0x12, 0x14, 0x08, 0x15, 0x12, 0x0D], // &
    [0x04, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00], // '
    [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02], // (
    [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08], // )
    [0x00, 0x0A, 0x04, 0x1F, 0x04, 0x0A, 0x00], // *
    [0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00], // +
    [0x00, 0x00, 0x00, 0x00, 0x04, 0x04, 0x08], // ,
    [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00], // -
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C], // .
    [0x01, 0x02, 0x04, 0x08, 0x10, 0x00, 0x00], // /
    [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E], // 0
    [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E], // 1
    [0x0E, 0x11, 0x01, 0x06, 0x08, 0x10, 0x1F], // 2
    [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E], // 3
    [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02], // 4
    [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E], // 5
    [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E], // 6
    [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08], // 7
    [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E], // 8
    [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C], // 9
    [0x00, 0x04, 0x00, 0x00, 0x00, 0x04, 0x00], // :
    [0x00, 0x04, 0x00, 0x00, 0x04, 0x04, 0x08], // ;
    [0x02, 0x04, 0x08, 0x10, 0x08, 0x04, 0x02], // <
    [0x00, 0x00, 0x1F, 0x00, 0x1F, 0x00, 0x00], // =
    [0x08, 0x04, 0x02, 0x01, 0x02, 0x04, 0x08], // >
    [0x0E, 0x11, 0x01, 0x06, 0x04, 0x00, 0x04], // ?
    [0x0E, 0x11, 0x17, 0x15, 0x17, 0x10, 0x0E], // @
    [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11], // A
    [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E], // B
    [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E], // C
    [0x1C, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1C], // D
    [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F], // E
    [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10], // F
    [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F], // G
    [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11], // H
    [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E], // I
    [0x07, 0x02, 0x02, 0x02, 0x12, 0x12, 0x0C], // J
    [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11], // K
    [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F], // L
    [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11], // M
    [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11], // N
    [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E], // O
    [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10], // P
    [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D], // Q
    [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11], // R
    [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E], // S
    [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04], // T
    [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E], // U
    [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04], // V
    [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11], // W
    [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11], // X
    [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04], // Y
    [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F], // Z
    [0x0E, 0x08, 0x08, 0x08, 0x08, 0x08, 0x0E], // [
    [0x10, 0x08, 0x04, 0x02, 0x01, 0x00, 0x00], // \
    [0x0E, 0x02, 0x02, 0x02, 0x02, 0x02, 0x0E], // ]
    [0x04, 0x0A, 0x11, 0x00, 0x00, 0x00, 0x00], // ^
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F], // _
    [0x08, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00], // `
    [0x00, 0x00, 0x0E, 0x01, 0x0F, 0x11, 0x0F], // a
    [0x10, 0x10, 0x1E, 0x11, 0x11, 0x11, 0x1E], // b
    [0x00, 0x00, 0x0E, 0x10, 0x10, 0x11, 0x0E], // c
    [0x01, 0x01, 0x0F, 0x11, 0x11, 0x11, 0x0F], // d
    [0x00, 0x00, 0x0E, 0x11, 0x1F, 0x10, 0x0E], // e
    [0x06, 0x08, 0x1E, 0x08, 0x08, 0x08, 0x08], // f
    [0x00, 0x00, 0x0F, 0x11, 0x0F, 0x01, 0x0E], // g
    [0x10, 0x10, 0x1E, 0x11, 0x11, 0x11, 0x11], // h
    [0x04, 0x00, 0x0C, 0x04, 0x04, 0x04, 0x0E], // i
    [0x02, 0x00, 0x06, 0x02, 0x02, 0x12, 0x0C], // j
    [0x10, 0x10, 0x12, 0x14, 0x18, 0x14, 0x12], // k
    [0x0C, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E], // l
    [0x00, 0x00, 0x1A, 0x15, 0x15, 0x11, 0x11], // m
    [0x00, 0x00, 0x1E, 0x11, 0x11, 0x11, 0x11], // n
    [0x00, 0x00, 0x0E, 0x11, 0x11, 0x11, 0x0E], // o
    [0x00, 0x00, 0x1E, 0x11, 0x1E, 0x10, 0x10], // p
    [0x00, 0x00, 0x0F, 0x11, 0x0F, 0x01, 0x01], // q
    [0x00, 0x00, 0x16, 0x19, 0x10, 0x10, 0x10], // r
    [0x00, 0x00, 0x0F, 0x10, 0x0E, 0x01, 0x1E], // s
    [0x08, 0x08, 0x1E, 0x08, 0x08, 0x09, 0x06], // t
    [0x00, 0x00, 0x11, 0x11, 0x11, 0x13, 0x0D], // u
    [0x00, 0x00, 0x11, 0x11, 0x11, 0x0A, 0x04], // v
    [0x00, 0x00, 0x11, 0x11, 0x15, 0x15, 0x0A], // w
    [0x00, 0x00, 0x11, 0x0A, 0x04, 0x0A, 0x11], // x
    [0x00, 0x00, 0x11, 0x11, 0x0F, 0x01, 0x0E], // y
    [0x00, 0x00, 0x1F, 0x02, 0x04, 0x08, 0x1F], // z
    [0x02, 0x04, 0x04, 0x08, 0x04, 0x04, 0x02], // {
    [0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04], // |
    [0x08, 0x04, 0x04, 0x02, 0x04, 0x04, 0x08], // }
    [0x00, 0x08, 0x15, 0x02, 0x00, 0x00, 0x00], // ~
];

fn draw_ascii_fallback(
    image: &mut RgbImage,
    byte: u8,
    x0: u32,
    y0: u32,
    px_w: u32,
    px_h: u32,
    fg: [u8; 3],
) {
    if !(0x21..=0x7E).contains(&byte) {
        return;
    }
    let rows = ASCII_5X7[usize::from(byte - 0x21)];
    let ox = x0 + px_w.saturating_sub(5) / 2;
    let oy = y0 + px_h.saturating_sub(14) / 2;
    for (row_idx, &bits) in rows.iter().enumerate() {
        let dy = u32::try_from(row_idx).unwrap_or(0) * 2;
        for col_idx in 0..5_u32 {
            if (bits & (1 << (4 - col_idx))) != 0 {
                fill_rect(image, ox + col_idx, oy + dy, 1, 2, fg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use image::GenericImageView;

    use super::*;
    use crate::tui_driver::emulator::TerminalEmulator;

    fn count_non_bg_pixels(img: &RgbImage, x0: u32, y0: u32, w: u32, h: u32) -> usize {
        let mut count = 0;
        for y in y0..(y0 + h).min(img.height()) {
            for x in x0..(x0 + w).min(img.width()) {
                if img.get_pixel(x, y).0 != DEFAULT_BG {
                    count += 1;
                }
            }
        }
        count
    }

    #[test]
    fn rasterizes_box_drawing_blocks_braille_ascii_and_cjk_to_valid_png() {
        let mut emu = TerminalEmulator::new(36, 8);
        emu.feed(
            concat!(
                "\x1b[1;1H┌─ TUI Status ─────────────────────┐",
                "\x1b[2;1H│ ASCII: ok  \x1b[7mSelected\x1b[0m              │",
                "\x1b[3;1H│ Blocks: ▀▄█▌▐░▒▓  Braille: ⣿⠿    │",
                "\x1b[4;1H│ 中文: 正常                       │",
                "\x1b[5;1H│ 日本語: 完了                     │",
                "\x1b[6;1H│ 한국어: 정상                     │",
                "\x1b[7;1H└──────────────────────────────────┘",
            )
            .as_bytes(),
        );
        let snap = emu.snapshot();
        let rasterizer = CellRasterizer::new();
        let png = rasterizer.render_png(&snap).expect("encode png");

        let decoded = image::load_from_memory(&png).expect("decode rendered png");
        assert_eq!(
            decoded.dimensions(),
            (
                u32::from(snap.cols) * DEFAULT_CELL_WIDTH,
                u32::from(snap.rows) * DEFAULT_CELL_HEIGHT,
            )
        );
        let rgb = decoded.to_rgb8();

        // Top-left box corner '┌' at cell (0, 0) connects cleanly to right edge and bottom edge.
        let cx = DEFAULT_CELL_WIDTH / 2;
        let cy = DEFAULT_CELL_HEIGHT / 2;
        assert_eq!(rgb.get_pixel(DEFAULT_CELL_WIDTH - 1, cy).0, DEFAULT_FG);
        assert_eq!(rgb.get_pixel(cx, DEFAULT_CELL_HEIGHT - 1).0, DEFAULT_FG);

        // Full block '█' at row 2, col 12 fills the whole cell with foreground.
        let block_x = 12 * DEFAULT_CELL_WIDTH;
        let block_y = 2 * DEFAULT_CELL_HEIGHT;
        assert_eq!(
            count_non_bg_pixels(
                &rgb,
                block_x,
                block_y,
                DEFAULT_CELL_WIDTH,
                DEFAULT_CELL_HEIGHT
            ),
            (DEFAULT_CELL_WIDTH * DEFAULT_CELL_HEIGHT) as usize
        );

        // Braille '⣿' at row 2, col 29 draws all 8 dots.
        let braille_x = 29 * DEFAULT_CELL_WIDTH;
        let braille_y = 2 * DEFAULT_CELL_HEIGHT;
        assert!(
            count_non_bg_pixels(
                &rgb,
                braille_x,
                braille_y,
                DEFAULT_CELL_WIDTH,
                DEFAULT_CELL_HEIGHT
            ) >= 8
        );

        // Reverse-video "Selected" cell at row 1, col 13 has inverted background pixels.
        let sel_x = 13 * DEFAULT_CELL_WIDTH;
        let sel_y = DEFAULT_CELL_HEIGHT;
        assert!(
            count_non_bg_pixels(&rgb, sel_x, sel_y, DEFAULT_CELL_WIDTH, DEFAULT_CELL_HEIGHT) > 20
        );

        // On hosts with system CJK fonts (such as macOS), Chinese, Japanese, and Korean
        // double-width cells each produce non-background glyph pixels.
        if !rasterizer.catalog.all_faces.is_empty() {
            for (label, row_idx) in [("Chinese", 3_u32), ("Japanese", 4_u32), ("Korean", 5_u32)] {
                let cjk_x = 2 * DEFAULT_CELL_WIDTH;
                let cjk_y = row_idx * DEFAULT_CELL_HEIGHT;
                let lit = count_non_bg_pixels(
                    &rgb,
                    cjk_x,
                    cjk_y,
                    DEFAULT_CELL_WIDTH * 2,
                    DEFAULT_CELL_HEIGHT,
                );
                assert!(lit > 0, "{label} glyph at row {row_idx} produced no pixels");
            }
        }
    }

    #[test]
    fn ascii_bitmap_fallback_renders_when_no_system_fonts_loaded() {
        let mut emu = TerminalEmulator::new(10, 2);
        emu.feed(b"Hello!");
        let snap = emu.snapshot();
        let rasterizer = CellRasterizer::without_system_fonts();
        let rgb = rasterizer.render_image(&snap);
        let lit = count_non_bg_pixels(&rgb, 0, 0, DEFAULT_CELL_WIDTH, DEFAULT_CELL_HEIGHT);
        assert!(lit > 10, "expected fallback pixels for 'H', got {lit}");
    }
}
