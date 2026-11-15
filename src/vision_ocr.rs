//! On-device OCR and conditional downscaling for text-dense images.
//!
//! Text-dense images (screenshots, document pages) are recognized with
//! `waterkit-vision`, downscaled to cut image token cost while preserving the
//! image attachment, and paired with a `<recognized_text>` block rendered
//! through Askama. Non-text-dense photographs and unsupported platforms pass
//! through unchanged.

use std::collections::HashSet;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(feature = "vision-ocr")]
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use aither_core::llm::tool::ToolResultPart;
use aither_core::llm::{Attachment, CommandStream, ToolCallId, ToolResult};
use askama::Template;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use image::{DynamicImage, GenericImageView, ImageFormat};
#[cfg(feature = "vision-ocr")]
use waterkit_vision::{Image as VisionImage, RecognitionLevel, RecognizeText, TextLine, Vision};

use crate::ipc::CallImageStore;

/// Caps text-dense images at 768px on the longest edge so vision providers
/// spend fewer image tiles/tokens while retaining layout alongside `<recognized_text>`.
pub const OCR_DOWNSCALE_MAX_DIM: u32 = 768;

/// Minimum confidence to keep a recognized line when judging text density.
#[cfg(feature = "vision-ocr")]
const MIN_LINE_CONFIDENCE: f32 = 0.3;

/// Summary metrics extracted from `RecognizeText` output used to classify text density.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextDensityMetrics {
    /// Count of non-empty kept text lines.
    pub line_count: usize,
    /// Total non-whitespace characters across kept lines.
    pub char_count: usize,
    /// Sum of normalized line bounding-box areas clamped to `1.0`.
    pub text_area: f32,
}

impl TextDensityMetrics {
    /// Whether the recognized lines indicate a text-dense image (screenshot / document page)
    /// rather than a casual photo with incidental sign text.
    #[must_use]
    pub fn is_text_dense(self) -> bool {
        (self.line_count >= 3 && self.char_count >= 30)
            || (self.line_count >= 2 && self.char_count >= 24 && self.text_area >= 0.025)
            || self.char_count >= 60
    }
}

/// Askama template rendering the extracted text section accompanying a downscaled image.
#[derive(Debug, Template)]
#[template(path = "recognized_text.txt", escape = "none")]
pub struct RecognizedTextTemplate<'a> {
    /// Human-readable source label (e.g. file name or command name).
    pub source: &'a str,
    /// Recognized text lines joined with newlines.
    pub text: &'a str,
}

/// Result of running OCR classification on an encoded image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OcrOutcome {
    /// Photo or unsupported platform: keep the original image bytes & format unchanged.
    Unchanged,
    /// Text-dense image: attach the downscaled image bytes alongside recognized text.
    TextDense {
        /// Rendered `<recognized_text>` section ready for model context.
        rendered_text: String,
        /// Resized encoded image bytes (never upscaled; equals original bytes if already within budget).
        image_bytes: Vec<u8>,
        /// Media type of `image_bytes`.
        media_type: String,
    },
}

/// Shared, lazily initialized native OCR runtime (holds no global state).
#[derive(Debug, Clone, Default)]
pub struct VisionOcr {
    #[cfg(feature = "vision-ocr")]
    inner: Arc<Mutex<VisionRuntimeState>>,
    next_output_id: Arc<AtomicU64>,
}

#[cfg(feature = "vision-ocr")]
#[derive(Debug, Default)]
struct VisionRuntimeState {
    vision: Option<Vision>,
    unavailable: bool,
}

impl VisionOcr {
    /// Creates a new OCR runner whose native backend initializes on first use.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the active build feature and host OS support native text recognition.
    #[must_use]
    pub const fn is_supported(&self) -> bool {
        cfg!(all(
            feature = "vision-ocr",
            any(
                target_os = "macos",
                target_os = "ios",
                target_os = "windows",
                target_os = "android",
            )
        ))
    }

    /// Reads `path` and inspects it off the event loop with native OCR.
    pub async fn process_file(
        &self,
        path: &Path,
        media_type: &str,
        source_label: &str,
    ) -> OcrOutcome {
        if !media_type.starts_with("image/") || !self.is_supported() {
            return OcrOutcome::Unchanged;
        }
        let Ok(bytes) = async_fs::read(path).await else {
            return OcrOutcome::Unchanged;
        };
        self.process_bytes(bytes, media_type.to_owned(), source_label.to_owned())
            .await
    }

    /// Inspects encoded `bytes` off the event loop (`blocking::unblock`), classifying text density
    /// from the `RecognizeText` result itself and downscaling only text-dense images.
    pub async fn process_bytes(
        &self,
        bytes: Vec<u8>,
        media_type: String,
        source_label: String,
    ) -> OcrOutcome {
        if !media_type.starts_with("image/") || !self.is_supported() || bytes.is_empty() {
            return OcrOutcome::Unchanged;
        }
        let ocr = self.clone();
        blocking::unblock(move || ocr.process_bytes_blocking(&bytes, &media_type, &source_label))
            .await
    }

    #[cfg(feature = "vision-ocr")]
    fn process_bytes_blocking(
        &self,
        bytes: &[u8],
        media_type: &str,
        source_label: &str,
    ) -> OcrOutcome {
        let lines = match self.recognize_lines(bytes) {
            Ok(lines) => lines,
            Err(reason) => {
                tracing::warn!(%reason, "native OCR skipped; leaving image unchanged");
                return OcrOutcome::Unchanged;
            }
        };
        let Some((recognized, metrics)) = summarize_recognized_lines(&lines) else {
            return OcrOutcome::Unchanged;
        };
        if !metrics.is_text_dense() {
            return OcrOutcome::Unchanged;
        }
        let (image_bytes, out_media_type) = downscale_if_needed(bytes, media_type);
        let Ok(rendered_text) = (RecognizedTextTemplate {
            source: source_label,
            text: &recognized,
        })
        .render() else {
            return OcrOutcome::Unchanged;
        };
        OcrOutcome::TextDense {
            rendered_text,
            image_bytes,
            media_type: out_media_type,
        }
    }

    #[cfg(not(feature = "vision-ocr"))]
    fn process_bytes_blocking(
        &self,
        _bytes: &[u8],
        _media_type: &str,
        _source_label: &str,
    ) -> OcrOutcome {
        OcrOutcome::Unchanged
    }

    #[cfg(feature = "vision-ocr")]
    fn recognize_lines(&self, bytes: &[u8]) -> Result<Vec<TextLine>, String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "vision OCR state mutex poisoned".to_owned())?;
        if guard.unavailable {
            return Err("native text recognition unavailable on this device".to_owned());
        }
        if guard.vision.is_none() {
            let vision = create_headless_vision()?;
            guard.vision = Some(vision);
        }
        let vision = guard
            .vision
            .as_ref()
            .ok_or_else(|| "headless vision missing after init".to_owned())?;
        if vision.capabilities().text.native.is_empty() {
            guard.unavailable = true;
            return Err("native text capabilities empty".to_owned());
        }
        let image = VisionImage::from_encoded(bytes.to_vec().into());
        // Apple's `capabilities().text.native` intersects `Fast` and `Accurate`,
        // omitting CJK scripts only present in `Accurate`; passing all 33 languages
        // drops CJK lines, while auto-detect can drop Japanese on headless VMs.
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let request = RecognizeText::new()
            .level(RecognitionLevel::Accurate)
            .languages(
                [
                    "en-Latn-US",
                    "zh-Hans",
                    "zh-Hant",
                    "ja-Jpan-JP",
                    "ko-Kore-KR",
                ]
                .into_iter()
                .filter_map(|tag| tag.parse().ok()),
            );
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        let request = RecognizeText::new().level(RecognitionLevel::Accurate);
        let result = futures_lite::future::block_on(vision.perform(&image, &request));
        drop(guard);
        result.map_err(|error| format!("vision.perform failed: {error}"))
    }

    async fn attachment_from_bytes(
        &self,
        outputs_dir: Option<&Path>,
        bytes: &[u8],
        media_type: &str,
    ) -> Option<Attachment> {
        let parsed_mime: mime::Mime = media_type.parse().ok()?;
        if let Some(dir) = outputs_dir
            && async_fs::create_dir_all(dir).await.is_ok()
        {
            let seq = self.next_output_id.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("ocr-{seq}.{}", ext_for_mime(media_type)));
            if async_fs::write(&path, bytes).await.is_ok()
                && let Some(url) = file_url(&path).await
            {
                return Some(Attachment::new(url, parsed_mime));
            }
        }
        let url = data_url(bytes, media_type)?;
        Some(Attachment::new(url, parsed_mime))
    }
}

/// Inspects command-recorded and inline images returned by a tool call,
/// attaching every image to `Message::Tool` while running native OCR and
/// conditional downscaling on text-dense images.
pub(crate) async fn augment_tool_outcome(
    ocr: &VisionOcr,
    call_images: Option<&CallImageStore>,
    outputs_dir: Option<&Path>,
    call_id: &ToolCallId,
    tool_name: &str,
    result: ToolResult,
) -> Option<(ToolResult, Vec<Attachment>)> {
    let mut attachments = Vec::new();
    let mut recognized = Vec::new();
    let mut seen_paths = HashSet::new();

    if let Some(store) = call_images {
        for image in store.take(call_id) {
            seen_paths.insert(image.path.clone());
            inspect_file_image(
                ocr,
                outputs_dir,
                &image.path,
                &image.media_type,
                &image.source,
                &mut attachments,
                &mut recognized,
            )
            .await;
        }
    }

    match &result {
        ToolResult::Command(cmd) => {
            for stream in std::iter::once(&cmd.stdout).chain(cmd.stderr.as_ref()) {
                if let CommandStream::Image { media_type, file } = stream {
                    let path = PathBuf::from(&file.path);
                    if media_type.starts_with("image/") && seen_paths.insert(path.clone()) {
                        inspect_file_image(
                            ocr,
                            outputs_dir,
                            &path,
                            media_type,
                            tool_name,
                            &mut attachments,
                            &mut recognized,
                        )
                        .await;
                    }
                }
            }
        }
        ToolResult::Binary { mime, content } if mime.starts_with("image/") => {
            inspect_bytes_image(
                ocr,
                outputs_dir,
                content,
                mime,
                tool_name,
                &mut attachments,
                &mut recognized,
            )
            .await;
        }
        ToolResult::Parts { parts } => {
            for part in parts {
                if let ToolResultPart::Binary { mime, content } = part
                    && mime.starts_with("image/")
                {
                    inspect_bytes_image(
                        ocr,
                        outputs_dir,
                        content,
                        mime,
                        tool_name,
                        &mut attachments,
                        &mut recognized,
                    )
                    .await;
                }
            }
        }
        _ => {}
    }

    if attachments.is_empty() && recognized.is_empty() {
        return None;
    }

    Some((append_recognized_sections(result, recognized), attachments))
}

async fn inspect_file_image(
    ocr: &VisionOcr,
    outputs_dir: Option<&Path>,
    path: &Path,
    media_type: &str,
    source_label: &str,
    attachments: &mut Vec<Attachment>,
    recognized: &mut Vec<String>,
) {
    match ocr.process_file(path, media_type, source_label).await {
        OcrOutcome::TextDense {
            rendered_text,
            image_bytes,
            media_type: out_mime,
        } => {
            recognized.push(rendered_text);
            if let Some(attachment) = ocr
                .attachment_from_bytes(outputs_dir, &image_bytes, &out_mime)
                .await
            {
                attachments.push(attachment);
            }
        }
        OcrOutcome::Unchanged => {
            if let Some(attachment) = file_or_data_attachment(path, media_type).await {
                attachments.push(attachment);
            }
        }
    }
}

async fn inspect_bytes_image(
    ocr: &VisionOcr,
    outputs_dir: Option<&Path>,
    bytes: &[u8],
    media_type: &str,
    source_label: &str,
    attachments: &mut Vec<Attachment>,
    recognized: &mut Vec<String>,
) {
    match ocr
        .process_bytes(
            bytes.to_vec(),
            media_type.to_owned(),
            source_label.to_owned(),
        )
        .await
    {
        OcrOutcome::TextDense {
            rendered_text,
            image_bytes,
            media_type: out_mime,
        } => {
            recognized.push(rendered_text);
            if let Some(attachment) = ocr
                .attachment_from_bytes(outputs_dir, &image_bytes, &out_mime)
                .await
            {
                attachments.push(attachment);
            }
        }
        OcrOutcome::Unchanged => {
            if let Some(attachment) = ocr
                .attachment_from_bytes(outputs_dir, bytes, media_type)
                .await
            {
                attachments.push(attachment);
            }
        }
    }
}

fn append_recognized_sections(result: ToolResult, recognized: Vec<String>) -> ToolResult {
    if recognized.is_empty() {
        return result;
    }
    let mut parts = match result {
        ToolResult::Done => Vec::new(),
        ToolResult::Parts { parts } => parts,
        ToolResult::Text { text } => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![ToolResultPart::Text { text }]
            }
        }
        ToolResult::Tsv { text } => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![ToolResultPart::Tsv { text }]
            }
        }
        ToolResult::Binary { mime, content } => vec![ToolResultPart::Binary { mime, content }],
        other @ (ToolResult::Command(_) | ToolResult::Error { .. }) => {
            let rendered = other.render_for_model();
            if rendered.is_empty() {
                Vec::new()
            } else {
                vec![ToolResultPart::Text { text: rendered }]
            }
        }
    };
    parts.extend(
        recognized
            .into_iter()
            .map(|text| ToolResultPart::Text { text }),
    );
    ToolResult::parts(parts)
}

async fn file_url(path: &Path) -> Option<url::Url> {
    if let Ok(canonical) = async_fs::canonicalize(path).await
        && let Ok(url) = url::Url::from_file_path(canonical)
    {
        return Some(url);
    }
    let abs = std::path::absolute(path).ok()?;
    url::Url::from_file_path(abs).ok()
}

fn data_url(bytes: &[u8], media_type: &str) -> Option<url::Url> {
    let encoded = BASE64.encode(bytes);
    url::Url::parse(&format!("data:{media_type};base64,{encoded}")).ok()
}

async fn file_or_data_attachment(path: &Path, media_type: &str) -> Option<Attachment> {
    let parsed_mime: mime::Mime = media_type.parse().ok()?;
    if let Some(url) = file_url(path).await {
        return Some(Attachment::new(url, parsed_mime));
    }
    let bytes = async_fs::read(path).await.ok()?;
    let url = data_url(&bytes, media_type)?;
    Some(Attachment::new(url, parsed_mime))
}

/// Renders `<recognized_text>` directly from already-recognized text via the Askama template.
///
/// Exposed for tests and callers formatting precomputed recognition results.
///
/// # Errors
/// Returns an [`askama::Error`] only if template formatting fails.
pub fn render_recognized_text(source: &str, text: &str) -> Result<String, askama::Error> {
    RecognizedTextTemplate { source, text }.render()
}

/// Evaluates kept text lines and normalized bounding-box area from `waterkit-vision` lines.
#[cfg(feature = "vision-ocr")]
#[must_use]
pub fn summarize_recognized_lines(lines: &[TextLine]) -> Option<(String, TextDensityMetrics)> {
    let entries: Vec<(&str, f32)> = lines
        .iter()
        .filter(|line| line.confidence.is_none_or(|c| c >= MIN_LINE_CONFIDENCE))
        .filter_map(|line| {
            let trimmed = line.text.trim();
            if trimmed.is_empty() {
                return None;
            }
            Some((trimmed, quad_area(&line.bounds)))
        })
        .collect();
    summarize_text_entries(&entries)
}

/// Computes normalized bounding-box area for a [`waterkit_vision::Quad`].
#[cfg(feature = "vision-ocr")]
fn quad_area(quad: &waterkit_vision::Quad) -> f32 {
    let xs = quad.0.iter().map(|p| p.x);
    let ys = quad.0.iter().map(|p| p.y);
    let min_x = xs.clone().fold(1.0_f32, f32::min);
    let max_x = xs.fold(0.0_f32, f32::max);
    let min_y = ys.clone().fold(1.0_f32, f32::min);
    let max_y = ys.fold(0.0_f32, f32::max);
    (max_x - min_x).clamp(0.0, 1.0) * (max_y - min_y).clamp(0.0, 1.0)
}

/// Summarizes `(text, normalized_area)` pairs into joined lines and [`TextDensityMetrics`].
#[must_use]
pub fn summarize_text_entries(entries: &[(&str, f32)]) -> Option<(String, TextDensityMetrics)> {
    let mut kept = Vec::new();
    let mut char_count = 0usize;
    let mut text_area = 0.0_f32;
    for &(raw, area) in entries {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        kept.push(trimmed);
        char_count += trimmed.chars().filter(|c| !c.is_whitespace()).count();
        text_area += area.clamp(0.0, 1.0);
    }
    if kept.is_empty() {
        return None;
    }
    Some((
        kept.join("\n"),
        TextDensityMetrics {
            line_count: kept.len(),
            char_count,
            text_area: text_area.min(1.0),
        },
    ))
}

/// Downscales `bytes` so neither dimension exceeds [`OCR_DOWNSCALE_MAX_DIM`].
///
/// Images already within budget are returned unchanged; decode/encode failures also fall back
/// safely to the original bytes so the model still receives the image.
#[must_use]
pub fn downscale_if_needed(bytes: &[u8], media_type: &str) -> (Vec<u8>, String) {
    let Ok(decoded) = image::load_from_memory(bytes) else {
        return (bytes.to_vec(), media_type.to_owned());
    };
    let (width, height) = decoded.dimensions();
    if width <= OCR_DOWNSCALE_MAX_DIM && height <= OCR_DOWNSCALE_MAX_DIM {
        return (bytes.to_vec(), media_type.to_owned());
    }
    let resized = decoded.resize(
        OCR_DOWNSCALE_MAX_DIM,
        OCR_DOWNSCALE_MAX_DIM,
        image::imageops::FilterType::Triangle,
    );
    let (format, out_mime) = output_format_for_mime(media_type);
    encode_image(&resized, format).map_or_else(
        || (bytes.to_vec(), media_type.to_owned()),
        |encoded| (encoded, out_mime.to_owned()),
    )
}

/// File extension matching `media_type` for derived resized image files.
#[must_use]
pub fn ext_for_mime(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        _ => "png",
    }
}

fn output_format_for_mime(media_type: &str) -> (ImageFormat, &'static str) {
    match media_type {
        "image/jpeg" | "image/jpg" => (ImageFormat::Jpeg, "image/jpeg"),
        "image/webp" => (ImageFormat::WebP, "image/webp"),
        _ => (ImageFormat::Png, "image/png"),
    }
}

fn encode_image(img: &DynamicImage, format: ImageFormat) -> Option<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::new());
    img.write_to(&mut cursor, format).ok()?;
    Some(cursor.into_inner())
}

#[cfg(feature = "vision-ocr")]
fn create_headless_vision() -> Result<Vision, String> {
    let instance = waterkit_vision::wgpu::Instance::new(
        waterkit_vision::wgpu::InstanceDescriptor::new_without_display_handle(),
    );
    let adapter = futures_lite::future::block_on(instance.request_adapter(
        &waterkit_vision::wgpu::RequestAdapterOptions {
            power_preference: waterkit_vision::wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
            ..Default::default()
        },
    ))
    .map_err(|error| format!("wgpu request_adapter failed: {error}"))?;
    let (device, queue) = futures_lite::future::block_on(
        adapter.request_device(&waterkit_vision::wgpu::DeviceDescriptor::default()),
    )
    .map_err(|error| format!("wgpu request_device failed: {error}"))?;
    Ok(Vision::new(Arc::new(device), Arc::new(queue)))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb};

    pub fn make_photo_png(width: u32, height: u32) -> Vec<u8> {
        let mut buf: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::new(width, height);
        for (x, y, pixel) in buf.enumerate_pixels_mut() {
            // Smooth sunset-like sky/hill gradient with zero text features.
            #[allow(clippy::cast_possible_truncation)]
            let r = ((x * 255) / width.max(1)) as u8;
            #[allow(clippy::cast_possible_truncation)]
            let g = ((y * 180) / height.max(1)) as u8;
            let horizon = height * 2 / 3;
            let b = if y < horizon { 210 } else { 70 };
            *pixel = Rgb([r, g, b]);
        }
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(buf)
            .write_to(&mut out, ImageFormat::Png)
            .expect("encode photo png");
        out.into_inner()
    }

    #[cfg(all(feature = "vision-ocr", target_os = "macos"))]
    pub fn render_multilingual_text_png(lines: &[&str], width: usize, height: usize) -> Vec<u8> {
        use objc2::AnyThread;
        use objc2::rc::Retained;
        use objc2::runtime::AnyObject;
        use objc2_app_kit::{
            NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSDeviceRGBColorSpace, NSFont,
            NSFontAttributeName, NSForegroundColorAttributeName, NSGraphicsContext,
            NSStringDrawing,
        };
        use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};

        let rep: Retained<NSBitmapImageRep> = unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                std::ptr::null_mut(),
                width.cast_signed(),
                height.cast_signed(),
                8,
                4,
                true,
                false,
                NSDeviceRGBColorSpace,
                0,
                0,
            )
            .expect("allocate NSBitmapImageRep")
        };
        let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)
            .expect("bitmap graphics context");
        ctx.saveGraphicsState();
        NSGraphicsContext::setCurrentContext(Some(&ctx));

        NSColor::whiteColor().setFill();
        let canvas = NSRect::new(
            NSPoint::new(0.0, 0.0),
            #[allow(clippy::cast_precision_loss)]
            NSSize::new(width as f64, height as f64),
        );
        objc2_app_kit::NSBezierPath::fillRect(canvas);

        let font = NSFont::systemFontOfSize(34.0);
        let color = NSColor::blackColor();
        let attrs: Retained<NSDictionary<objc2_foundation::NSAttributedStringKey, AnyObject>> = unsafe {
            let keys = [NSFontAttributeName, NSForegroundColorAttributeName];
            let objs: [&AnyObject; 2] = [(*font).as_ref(), (*color).as_ref()];
            NSDictionary::from_slices(&keys, &objs)
        };

        for (idx, line) in lines.iter().enumerate() {
            let text = NSString::from_str(line);
            #[allow(clippy::cast_precision_loss)]
            let y = (idx as f64).mul_add(-64.0, (height as f64) - 96.0);
            let point = NSPoint::new(48.0, y);
            unsafe {
                text.drawAtPoint_withAttributes(point, Some(&attrs));
            }
        }

        ctx.flushGraphics();
        NSGraphicsContext::setCurrentContext(None);
        ctx.restoreGraphicsState();
        let empty = NSDictionary::new();
        let png_data = unsafe {
            rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &empty)
                .expect("encode png representation")
        };
        png_data.to_vec()
    }

    #[test]
    fn density_classifier_separates_documents_from_incidental_signs() {
        let sign = summarize_text_entries(&[("STOP", 0.008)]).expect("entries");
        assert!(!sign.1.is_text_dense(), "{:?}", sign.1);

        let document = summarize_text_entries(&[
            ("Cargo test failed in module billing", 0.02),
            ("Expected 200 OK but received 500", 0.02),
            ("中文测试 日本語テスト 한국어 테스트", 0.03),
        ])
        .expect("document entries");
        assert!(document.1.is_text_dense(), "{:?}", document.1);
    }

    #[test]
    fn downscale_reduces_dimensions_and_image_tokens() {
        let big = make_photo_png(1600, 1000);
        let orig_dims = image::load_from_memory(&big)
            .expect("decode original")
            .dimensions();
        let (resized, mime) = downscale_if_needed(&big, "image/png");
        assert_eq!(mime, "image/png");
        let new_dims = image::load_from_memory(&resized)
            .expect("decode resized")
            .dimensions();
        assert_eq!(new_dims, (768, 480));
        assert!(
            u64::from(new_dims.0) * u64::from(new_dims.1)
                < u64::from(orig_dims.0) * u64::from(orig_dims.1)
        );
    }

    #[test]
    fn recognized_text_template_renders_without_hand_concat() {
        let rendered =
            render_recognized_text("shot.png", "hello\n世界").expect("template renders cleanly");
        assert_eq!(
            rendered.trim_end(),
            "<recognized_text source=\"shot.png\">\nhello\n世界\n</recognized_text>"
        );
    }

    #[tokio::test]
    async fn unsupported_or_disabled_platform_sends_image_unchanged() {
        let ocr = VisionOcr::new();
        let png = make_photo_png(1200, 800);
        if !ocr.is_supported() {
            let outcome = ocr
                .process_bytes(png, "image/png".to_owned(), "shot.png".to_owned())
                .await;
            assert_eq!(outcome, OcrOutcome::Unchanged);
        }
    }

    #[cfg(all(feature = "vision-ocr", target_os = "macos"))]
    #[tokio::test]
    async fn recognizes_en_zh_ja_ko_text_dense_screenshot_and_leaves_photo_unchanged() {
        let ocr = VisionOcr::new();
        assert!(ocr.is_supported());

        let screenshot = render_multilingual_text_png(
            &[
                "Build log summary: 12 targets compiled",
                "中文界面状态：系统运行正常",
                "日本語ログ確認：接続完了しました",
                "한국어 상태 확인: 배포 준비 완료",
            ],
            1200,
            600,
        );
        let orig_pixels = 1200_u64 * 600_u64;
        let outcome = ocr
            .process_bytes(
                screenshot.clone(),
                "image/png".to_owned(),
                "screenshot.png".to_owned(),
            )
            .await;
        match outcome {
            OcrOutcome::TextDense {
                rendered_text,
                image_bytes,
                media_type,
            } => {
                assert_eq!(media_type, "image/png");
                assert!(rendered_text.contains("<recognized_text source=\"screenshot.png\">"));
                assert!(
                    rendered_text.contains("Build") || rendered_text.contains("12"),
                    "{rendered_text}"
                );
                assert!(rendered_text.contains("中文"), "{rendered_text}");
                assert!(rendered_text.contains("日本語"), "{rendered_text}");
                assert!(rendered_text.contains("한국어"), "{rendered_text}");

                let (w, h) = image::load_from_memory(&image_bytes)
                    .expect("valid resized PNG")
                    .dimensions();
                assert!(w <= OCR_DOWNSCALE_MAX_DIM && h <= OCR_DOWNSCALE_MAX_DIM);
                assert!(u64::from(w) * u64::from(h) < orig_pixels);
            }
            OcrOutcome::Unchanged => {
                let debug_lines = ocr.recognize_lines(&screenshot);
                panic!(
                    "expected multilingual screenshot to classify as text-dense; lines={debug_lines:?}"
                )
            }
        }

        let photo = make_photo_png(1200, 800);
        let photo_outcome = ocr
            .process_bytes(photo, "image/png".to_owned(), "photo.png".to_owned())
            .await;
        assert_eq!(photo_outcome, OcrOutcome::Unchanged);
    }

    #[cfg(all(feature = "vision-ocr", target_os = "macos"))]
    #[tokio::test]
    async fn child_hooks_forward_ocr_and_downscaled_images() {
        use std::borrow::Cow;
        use std::collections::VecDeque;
        use std::ops::ControlFlow;
        use std::sync::Mutex;
        use std::sync::atomic::AtomicBool;

        use aither_agent::Agent;
        use aither_core::llm::model::Profile;
        use aither_core::llm::{
            Classify, Event, Failure, LLMRequest, LanguageModel, Role, Tool, ToolCall, ToolCallId,
            ToolContext, ToolResult,
        };
        use futures_lite::Stream;
        use schemars::JsonSchema;
        use serde::Deserialize;

        use crate::ipc::RecordedCallImage;
        use crate::subagent::ChildHooks;

        #[derive(Debug)]
        struct StubErr;
        impl std::fmt::Display for StubErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("stub error")
            }
        }
        impl std::error::Error for StubErr {}
        impl Classify for StubErr {
            fn failure(&self) -> Failure {
                Failure::permanent()
            }
        }

        #[derive(Clone)]
        struct ScriptedModel {
            script: Arc<Mutex<VecDeque<Vec<Event>>>>,
            requests: Arc<Mutex<Vec<LLMRequest>>>,
        }

        impl LanguageModel for ScriptedModel {
            type Error = StubErr;

            fn respond(
                &self,
                request: LLMRequest,
            ) -> impl Stream<Item = Result<Event, Self::Error>> + Send {
                self.requests.lock().expect("requests").push(request);
                let events = self
                    .script
                    .lock()
                    .expect("script")
                    .pop_front()
                    .expect("scripted turn");
                futures_lite::stream::iter(events.into_iter().map(Ok))
            }

            fn profile(
                &self,
            ) -> impl std::future::Future<Output = Result<Profile, Self::Error>> + Send
            {
                std::future::ready(Ok(Profile::new(
                    "scripted",
                    "test",
                    "scripted",
                    "scripted model",
                    128_000,
                )))
            }
        }

        #[derive(Debug, Deserialize, JsonSchema)]
        struct NoArgs {}

        struct CaptureTool {
            store: CallImageStore,
            images: Vec<RecordedCallImage>,
        }

        impl Tool for CaptureTool {
            fn name(&self) -> Cow<'static, str> {
                Cow::Borrowed("terminal")
            }

            fn description(&self) -> Cow<'static, str> {
                Cow::Borrowed("Captures a test screenshot.")
            }

            type Arguments = NoArgs;
            type Res = ToolResult;

            fn call(
                &self,
                _args: Self::Arguments,
                cx: ToolContext,
            ) -> impl std::future::Future<Output = aither_core::Result<Self::Res>> + Send
            {
                if let Some(call) = cx.identity() {
                    self.store.record(call.call(), self.images.clone());
                }
                std::future::ready(Ok(ToolResult::text("Screenshot captured.")))
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let screenshot_path = dir.path().join("screen.png");
        let photo_path = dir.path().join("photo.png");
        let screenshot = render_multilingual_text_png(
            &[
                "Build status: 12 targets succeeded",
                "中文界面状态：所有编译与集成测试全部通过",
                "日本語確認：ビルド成功しました",
                "한국어 확인: 배포 준비 완료",
            ],
            1200,
            600,
        );
        let photo = make_photo_png(1200, 800);
        async_fs::write(&screenshot_path, &screenshot)
            .await
            .expect("write screenshot");
        async_fs::write(&photo_path, &photo)
            .await
            .expect("write photo");

        let ocr = VisionOcr::new();
        let store = CallImageStore::default();
        let outputs_dir = dir.path().join("outputs");
        let hooks = ChildHooks::compacting(None, Arc::new(AtomicBool::new(false))).with_image_ocr(
            ocr.clone(),
            store.clone(),
            outputs_dir.clone(),
        );
        let recorded = vec![
            RecordedCallImage {
                path: screenshot_path.clone(),
                media_type: "image/png".to_owned(),
                source: "computer screenshot".to_owned(),
            },
            RecordedCallImage {
                path: photo_path.clone(),
                media_type: "image/png".to_owned(),
                source: "browser screenshot".to_owned(),
            },
        ];
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = ScriptedModel {
            script: Arc::new(Mutex::new(VecDeque::from([
                vec![Event::ToolCall(ToolCall::new(
                    "call-recorded",
                    "terminal",
                    serde_json::json!({}),
                ))],
                vec![Event::Text("done".to_owned())],
            ]))),
            requests: Arc::clone(&requests),
        };
        let mut agent = Agent::builder(llm)
            .tool(CaptureTool {
                store: store.clone(),
                images: recorded,
            })
            .hooks(hooks)
            .build();
        let _ = agent
            .run("inspect", std::iter::empty())
            .drive((), std::future::pending::<()>(), |(), _| async {
                ControlFlow::Continue(())
            })
            .await
            .expect("child agent completes");

        let tool_message = {
            let captured_requests = requests.lock().expect("requests");
            captured_requests[1]
                .messages()
                .iter()
                .find(|msg| msg.role() == Role::Tool && msg.tool_call_id() == Some("call-recorded"))
                .expect("tool message reaches model")
                .clone()
        };
        let attachments = tool_message.attachments();
        assert_eq!(attachments.len(), 2);
        let rendered = tool_message.content();
        assert!(rendered.contains("Screenshot captured."), "{rendered}");
        assert!(
            rendered.contains("<recognized_text source=\"computer screenshot\">"),
            "{rendered}"
        );
        assert!(rendered.contains("中文"), "{rendered}");
        assert!(rendered.contains("日本語"), "{rendered}");
        assert!(rendered.contains("한국어"), "{rendered}");

        let resized_path = attachments[0].url().to_file_path().expect("resized file");
        assert_ne!(
            resized_path,
            async_fs::canonicalize(&screenshot_path)
                .await
                .expect("canon screenshot")
        );
        let resized_bytes = async_fs::read(&resized_path).await.expect("read resized");
        let (w, h) = image::load_from_memory(&resized_bytes)
            .expect("decode resized")
            .dimensions();
        assert!(w <= OCR_DOWNSCALE_MAX_DIM && h <= OCR_DOWNSCALE_MAX_DIM);

        let unchanged_path = attachments[1].url().to_file_path().expect("photo file");
        assert_eq!(
            unchanged_path,
            async_fs::canonicalize(&photo_path)
                .await
                .expect("canon photo")
        );
        assert!(
            store.take(&ToolCallId::new("call-recorded")).is_empty(),
            "store entry was consumed"
        );
    }

    #[cfg(all(feature = "vision-ocr", target_os = "macos"))]
    #[tokio::test]
    async fn augment_tool_outcome_handles_command_image_and_inline_binary() {
        use aither_core::llm::{
            CommandOutcome, CommandResult, CommandStream, StoredFile, ToolCallId, ToolResult,
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let screenshot_path = dir.path().join("screen.png");
        let outputs_dir = dir.path().join("outputs");
        let screenshot = render_multilingual_text_png(
            &[
                "Build status: 12 targets succeeded",
                "中文界面状态：所有编译与集成测试全部通过",
                "日本語確認：ビルド成功しました",
                "한국어 확인: 배포 준비 완료",
            ],
            1200,
            600,
        );
        async_fs::write(&screenshot_path, &screenshot)
            .await
            .expect("write screenshot");

        let ocr = VisionOcr::new();
        let cmd_call = ToolCallId::new("call-cmd");
        let (cmd_result, cmd_attachments) = augment_tool_outcome(
            &ocr,
            None,
            Some(&outputs_dir),
            &cmd_call,
            "terminal",
            ToolResult::Command(Box::new(CommandResult {
                stdout: CommandStream::Image {
                    media_type: "image/png".to_owned(),
                    file: StoredFile {
                        url: "outputs/screen.png".to_owned(),
                        path: screenshot_path.display().to_string(),
                    },
                },
                stderr: None,
                outcome: CommandOutcome::Exited { code: 0 },
                interruptions: Vec::new(),
            })),
        )
        .await
        .expect("CommandStream::Image augmented");
        assert_eq!(cmd_attachments.len(), 1);
        assert!(
            cmd_result
                .render_for_model()
                .contains("<recognized_text source=\"terminal\">")
        );

        let bin_call = ToolCallId::new("call-bin");
        let (bin_result, bin_attachments) = augment_tool_outcome(
            &ocr,
            None,
            None,
            &bin_call,
            "mcp_screen",
            ToolResult::image(screenshot, "image/png"),
        )
        .await
        .expect("ToolResult::Binary augmented");
        assert_eq!(bin_attachments.len(), 1);
        assert!(
            bin_attachments[0]
                .url()
                .as_str()
                .starts_with("data:image/png;base64,"),
            "{}",
            bin_attachments[0].url()
        );
        assert!(
            bin_result
                .render_for_model()
                .contains("<recognized_text source=\"mcp_screen\">")
        );
    }
}
