//! Clipboard handling for image paste.

use std::io::Write;
use std::path::{Path, PathBuf};

use arboard::Clipboard;
use async_fs;
use base64::Engine;
use image::ImageFormat as ImgFormat;
use url::Url;

use crate::session::SessionPaths;

/// Image format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// PNG image.
    Png,
    /// JPEG image.
    Jpeg,
    /// GIF image.
    Gif,
    /// WebP image.
    Webp,
}

impl ImageFormat {
    /// Get file extension.
    #[must_use]
    pub const fn extension(&self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }

    /// Detect format from URL path suffix.
    #[must_use]
    pub fn from_url(url: &str) -> Option<Self> {
        let url = Url::parse(url).ok()?;
        Self::from_url_path(&url)
    }

    #[must_use]
    fn from_url_path(url: &Url) -> Option<Self> {
        let extension = std::path::Path::new(url.path())
            .extension()
            .and_then(|value| value.to_str())?
            .to_ascii_lowercase();
        match extension.as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "gif" => Some(Self::Gif),
            "webp" => Some(Self::Webp),
            _ => None,
        }
    }

    /// Detect format from content-type header.
    #[must_use]
    pub fn from_content_type(content_type: &str) -> Option<Self> {
        let lower = content_type.to_lowercase();
        if lower.contains("image/png") {
            Some(Self::Png)
        } else if lower.contains("image/jpeg") || lower.contains("image/jpg") {
            Some(Self::Jpeg)
        } else if lower.contains("image/gif") {
            Some(Self::Gif)
        } else if lower.contains("image/webp") {
            Some(Self::Webp)
        } else {
            None
        }
    }

    #[must_use]
    const fn from_image_format(format: ImgFormat) -> Option<Self> {
        match format {
            ImgFormat::Png => Some(Self::Png),
            ImgFormat::Jpeg => Some(Self::Jpeg),
            ImgFormat::Gif => Some(Self::Gif),
            ImgFormat::WebP => Some(Self::Webp),
            _ => None,
        }
    }

    #[must_use]
    fn from_magic_bytes(data: &[u8]) -> Option<Self> {
        image::guess_format(data)
            .ok()
            .and_then(Self::from_image_format)
    }
}

/// A clipboard image.
#[derive(Debug, Clone)]
pub struct ClipboardImage {
    /// Encoded image bytes.
    pub data: Vec<u8>,
    /// Format `data` is encoded in.
    pub format: ImageFormat,
}

/// Clipboard paste result (image or text).
#[derive(Debug, Clone)]
pub struct ClipboardPasteResult {
    image: Option<ClipboardImage>,
    text: Option<String>,
}

impl ClipboardPasteResult {
    /// Create a result containing an image.
    #[must_use]
    pub const fn image(image: ClipboardImage) -> Self {
        Self {
            image: Some(image),
            text: None,
        }
    }

    /// Create a result containing text.
    #[must_use]
    pub const fn text(text: String) -> Self {
        Self {
            image: None,
            text: Some(text),
        }
    }

    /// Create an empty result.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            image: None,
            text: None,
        }
    }

    /// Get the image, if present.
    #[must_use]
    pub const fn image_ref(&self) -> Option<&ClipboardImage> {
        self.image.as_ref()
    }

    /// Get the text, if present.
    #[must_use]
    pub fn text_ref(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Consume and return the parts.
    #[must_use]
    pub fn into_parts(self) -> (Option<ClipboardImage>, Option<String>) {
        (self.image, self.text)
    }

    /// Rebuild a result from its parts.
    #[must_use]
    pub const fn from_parts(image: Option<ClipboardImage>, text: Option<String>) -> Self {
        Self { image, text }
    }

    /// Check whether the result is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.image.is_none() && self.text.is_none()
    }
}

/// Image manager for handling pasted images.
#[derive(Debug)]
pub struct ImageManager {
    session_images_dir: PathBuf,
    counter: u32,
}

impl ImageManager {
    /// Create a new image manager.
    #[must_use]
    pub fn new(session_dir: &Path) -> Self {
        Self {
            session_images_dir: SessionPaths::new(session_dir.to_path_buf()).images_dir(),
            counter: 0,
        }
    }

    /// Generate the next image path.
    pub fn next_path(&mut self, format: ImageFormat) -> PathBuf {
        self.counter += 1;
        let filename = format!("img_{:04}.{}", self.counter, format.extension());
        self.session_images_dir.join(filename)
    }

    /// Save an image to disk.
    ///
    /// # Errors
    ///
    /// Returns [`ImageError::Io`] when creating the image directory or writing
    /// the file fails.
    pub async fn save(&mut self, image: &ClipboardImage) -> Result<PathBuf, ImageError> {
        // Ensure directory exists
        async_fs::create_dir_all(&self.session_images_dir).await?;

        let path = self.next_path(image.format);
        async_fs::write(&path, &image.data).await?;

        Ok(path)
    }
}

/// Image error.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Clipboard error.
    #[error("Clipboard error: {0}")]
    Clipboard(String),
    /// Image encoding error.
    #[error("Image encoding error: {0}")]
    Encoding(String),
    /// Download error.
    #[error("Download error: {0}")]
    Download(String),
    /// URL did not resolve to an image.
    #[error("URL did not resolve to an image: {0}")]
    NotImage(String),
}

/// Encode an RGBA image as PNG with fast compression — encoding time sits on
/// the paste latency path and the bytes are only persisted and uploaded.
fn encode_png(img: &image::RgbaImage) -> Result<Vec<u8>, ImageError> {
    use image::ImageEncoder;
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};

    let mut png_data = Vec::new();
    PngEncoder::new_with_quality(&mut png_data, CompressionType::Fast, FilterType::Adaptive)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| ImageError::Encoding(e.to_string()))?;
    Ok(png_data)
}

/// macOS: POSIX path of a file URL on the pasteboard.
///
/// Finder copies of files (including iCloud Drive) put `public.file-url`
/// plus often a `public.png` of the file *icon*. The icon must not win.
#[cfg(target_os = "macos")]
fn pasteboard_file_path() -> Option<PathBuf> {
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::{NSString, NSURL};

    let pasteboard = NSPasteboard::generalPasteboard();
    let data_type = NSString::from_str("public.file-url");
    let url = pasteboard.stringForType(&data_type)?;
    let nsurl = NSURL::URLWithString(&url)?;
    let resolved = nsurl.filePathURL().unwrap_or(nsurl);
    let path = resolved.path()?;
    let path = PathBuf::from(path.to_string());
    path.is_absolute().then_some(path)
}

/// macOS: read already-encoded image bytes straight off the pasteboard.
/// Screenshots and in-app copies usually offer `public.png` or `public.jpeg`,
/// which skips the decode → RGBA → re-encode round trip entirely.
/// Finder file copies are handled by [`pasteboard_file_path`] instead.
#[cfg(target_os = "macos")]
fn pasteboard_image() -> Option<ClipboardImage> {
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::NSString;

    // Only formats the preview decode path (`image` crate: png, jpeg) and
    // model uploads can use as-is; anything else goes through arboard, which
    // decodes it natively.
    const PASSTHROUGH: [(&str, ImageFormat); 2] = [
        ("public.png", ImageFormat::Png),
        ("public.jpeg", ImageFormat::Jpeg),
    ];
    let pasteboard = NSPasteboard::generalPasteboard();
    for (uti, format) in PASSTHROUGH {
        let data_type = NSString::from_str(uti);
        if let Some(data) = pasteboard.dataForType(&data_type)
            && !data.is_empty()
        {
            return Some(ClipboardImage {
                data: data.to_vec(),
                format,
            });
        }
    }
    None
}

/// Try to get an image from the clipboard.
///
/// # Errors
///
/// Returns [`ImageError::Clipboard`] when the clipboard cannot be opened or
/// read, or [`ImageError::Encoding`] when the image cannot be encoded as PNG
/// (including dimensions that do not fit into `u32`).
pub fn get_clipboard_image() -> Result<Option<ClipboardImage>, ImageError> {
    #[cfg(target_os = "macos")]
    if let Some(image) = pasteboard_image() {
        return Ok(Some(image));
    }

    let mut clipboard = Clipboard::new().map_err(|e| ImageError::Clipboard(e.to_string()))?;

    // Try to get image from clipboard
    let image_data = match clipboard.get_image() {
        Ok(img) => img,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(e) => return Err(ImageError::Clipboard(e.to_string())),
    };

    // Convert to PNG bytes
    let width = u32::try_from(image_data.width)
        .map_err(|_| ImageError::Encoding("clipboard image width exceeds u32".to_string()))?;
    let height = u32::try_from(image_data.height)
        .map_err(|_| ImageError::Encoding("clipboard image height exceeds u32".to_string()))?;

    // Create an image buffer from the raw bytes
    let img = image::RgbaImage::from_raw(width, height, image_data.bytes.into_owned())
        .ok_or_else(|| ImageError::Encoding("Failed to create image buffer".to_string()))?;

    Ok(Some(ClipboardImage {
        data: encode_png(&img)?,
        format: ImageFormat::Png,
    }))
}

/// Try to get text from the clipboard.
///
/// # Errors
///
/// Returns [`ImageError::Clipboard`] when the clipboard cannot be opened or
/// the text read fails for a reason other than missing content.
pub fn get_clipboard_text() -> Result<Option<String>, ImageError> {
    let mut clipboard = Clipboard::new().map_err(|e| ImageError::Clipboard(e.to_string()))?;

    match clipboard.get_text() {
        Ok(text) if !text.is_empty() => Ok(Some(text)),
        Ok(_) | Err(arboard::Error::ContentNotAvailable) => Ok(None),
        Err(e) => Err(ImageError::Clipboard(e.to_string())),
    }
}

/// If `text` names an image file, read it; otherwise `None` (treat as text).
/// A missing or unreadable path is text, not a clipboard error.
fn image_from_clipboard_text(text: &str) -> Result<Option<ClipboardImage>, ImageError> {
    let Some(path) = pasted_image_path_candidate(text) else {
        return Ok(None);
    };
    match read_pasted_image_file(&path) {
        Ok(image) => Ok(image),
        Err(ImageError::Io(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Try to read from the clipboard, preferring images over text.
///
/// A Finder file copy is resolved from its file URL (the file's bytes), not
/// from the pasteboard PNG — that PNG is often the document icon.
/// Clipboard text that is a path to an image file is read the same way a
/// terminal paste of that path is.
///
/// # Errors
///
/// Returns [`ImageError`] when reading the clipboard image or text fails.
pub fn read_clipboard() -> Result<ClipboardPasteResult, ImageError> {
    #[cfg(target_os = "macos")]
    if let Some(path) = pasteboard_file_path() {
        if let Ok(Some(image)) = read_pasted_image_file(&path) {
            return Ok(ClipboardPasteResult::image(image));
        }
        // Not an image, or the file could not be read. Skip the icon PNG
        // and surface the path as text.
        if let Some(text) = get_clipboard_text()? {
            return Ok(ClipboardPasteResult::text(text));
        }
        return Ok(ClipboardPasteResult::empty());
    }

    if let Some(image) = get_clipboard_image()? {
        return Ok(ClipboardPasteResult::image(image));
    }

    if let Some(text) = get_clipboard_text()? {
        if let Some(image) = image_from_clipboard_text(&text)? {
            return Ok(ClipboardPasteResult::image(image));
        }
        return Ok(ClipboardPasteResult::text(text));
    }

    Ok(ClipboardPasteResult::empty())
}

/// A pasted single-line payload that resolves to a filesystem path.
///
/// Accepts a `file://` URL, a `~`-relative or absolute path, quoted or
/// shell-escaped. `None` means "treat the payload as text"; existence and
/// content checks happen on the worker in [`read_pasted_image_file`].
#[must_use]
pub fn pasted_image_path_candidate(pasted: &str) -> Option<PathBuf> {
    let text = pasted.trim();
    if text.is_empty() || text.contains('\n') {
        return None;
    }
    if let Ok(url) = Url::parse(text) {
        return (url.scheme() == "file")
            .then(|| url.to_file_path().ok())
            .flatten();
    }
    let unescaped = text
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| text.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .map(str::to_string)
        .or_else(|| {
            shell_words::split(text)
                .ok()
                .filter(|words| words.len() == 1)
                .and_then(|mut words| words.pop())
        })?;
    let path = match unescaped.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()?.join(rest),
        None => PathBuf::from(unescaped),
    };
    path.is_absolute().then_some(path)
}

/// Read a pasted file reference as an image: `None` when it is not a
/// regular file, exceeds the size cap, or carries no supported image
/// magic bytes.
///
/// # Errors
///
/// Returns [`ImageError::Io`] when the file cannot be stat'ed or read.
pub fn read_pasted_image_file(path: &Path) -> Result<Option<ClipboardImage>, ImageError> {
    const MAX_BYTES: u64 = 64 * 1024 * 1024;
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return Ok(None);
    }
    let data = std::fs::read(path)?;
    Ok(ImageFormat::from_magic_bytes(&data).map(|format| ClipboardImage { data, format }))
}

/// Copy text to the system clipboard.
///
/// # Errors
///
/// Returns [`ImageError::Clipboard`] when the clipboard cannot be opened or
/// the write fails.
pub fn set_clipboard_text(text: &str) -> Result<(), ImageError> {
    let mut clipboard = Clipboard::new().map_err(|e| ImageError::Clipboard(e.to_string()))?;
    clipboard
        .set_text(text)
        .map_err(|e| ImageError::Clipboard(e.to_string()))
}

/// Parse a remote URL candidate from clipboard text.
#[must_use]
pub fn is_image_url(text: &str) -> Option<Url> {
    let trimmed = text.trim();
    let url = Url::parse(trimmed).ok()?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }
    Some(url)
}

/// Download an image from a URL.
///
/// # Errors
///
/// Returns [`ImageError::Download`] when the request fails or the response
/// status is not successful, or when the content-type header and the payload's
/// magic bytes disagree; [`ImageError::NotImage`] when the response is not a
/// supported image format.
pub async fn download_image(url: &Url) -> Result<ClipboardImage, ImageError> {
    let response = zenwave::get(url.as_str())
        .await
        .map_err(|e| ImageError::Download(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        return Err(ImageError::Download(format!(
            "HTTP {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("Unknown")
        )));
    }

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_lowercase();

    // A declared non-image type never reaches the body; an image or an
    // opaque type (absent, octet-stream) still needs the bytes probed.
    let declared_image = content_type.starts_with("image/");
    if !content_type.is_empty() && !declared_image && !content_type.contains("octet-stream") {
        return Err(ImageError::NotImage(format!(
            "response content-type '{content_type}' is not an image type"
        )));
    }

    let data = read_image_body(response.into_body(), declared_image).await?;

    let format_from_header = ImageFormat::from_content_type(&content_type);
    let format_from_bytes = ImageFormat::from_magic_bytes(data.as_ref());
    let format = match (format_from_header, format_from_bytes) {
        (Some(header), Some(bytes)) if header != bytes => {
            return Err(ImageError::Download(format!(
                "image response format mismatch for {url}: content-type={content_type} bytes={bytes:?}"
            )));
        }
        (Some(header), _) => header,
        (None, Some(bytes)) => bytes,
        (None, None) => {
            let path_hint = ImageFormat::from_url_path(url).map_or_else(
                || "none".to_string(),
                |format| format.extension().to_string(),
            );
            return Err(ImageError::NotImage(format!(
                "response content-type '{content_type}' and bytes are not a supported image format (path hint: {path_hint})"
            )));
        }
    };

    Ok(ClipboardImage { data, format })
}

/// Read a response body as image bytes, bounded and probed: when the
/// content type did not vouch for an image, the first chunk's magic bytes
/// must — a non-image body aborts there instead of being pulled whole.
async fn read_image_body(body: zenwave::Body, declared_image: bool) -> Result<Vec<u8>, ImageError> {
    use futures_lite::AsyncReadExt;

    const MAX_BODY: usize = 64 * 1024 * 1024;

    let mut reader = body.into_reader();
    let mut data = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    let mut first = true;
    loop {
        let n = reader
            .read(&mut chunk)
            .await
            .map_err(|e| ImageError::Download(e.to_string()))?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..n]);
        if first {
            first = false;
            if !declared_image && ImageFormat::from_magic_bytes(&data).is_none() {
                return Err(ImageError::NotImage(
                    "response body is not a supported image format".to_string(),
                ));
            }
        }
        if data.len() > MAX_BODY {
            return Err(ImageError::Download(format!(
                "image exceeds {MAX_BODY} bytes"
            )));
        }
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_copies_as_html_with_raw_markup_escaped() {
        assert_eq!(
            markdown_to_basic_html("**bold** and *italic* <b>x</b> & \"q\"\n\n- one\n- two"),
            "<p><strong>bold</strong> and <em>italic</em> &lt;b&gt;x&lt;/b&gt; &amp; \"q\"</p>\n<ul>\n<li>one</li>\n<li>two</li>\n</ul>\n"
        );
    }

    #[test]
    fn image_url_detection_accepts_http_and_https_only() {
        assert!(is_image_url("https://example.com/view?page=photo.png").is_some());
        assert!(is_image_url("https://cdn.example.com/signed/blob").is_some());
        assert!(is_image_url("file:///tmp/test.png").is_none());
        assert!(is_image_url("not a url").is_none());
    }

    #[test]
    fn format_detection_uses_url_path_extension_only() {
        let page_url =
            Url::parse("https://example.com/view?page=photo.png").expect("url should parse");
        assert!(ImageFormat::from_url_path(&page_url).is_none());

        let asset_url =
            Url::parse("https://example.com/assets/photo.png").expect("url should parse");
        assert_eq!(
            ImageFormat::from_url_path(&asset_url),
            Some(ImageFormat::Png)
        );
    }

    #[test]
    fn pasted_image_path_candidate_accepts_path_shapes() {
        assert_eq!(
            pasted_image_path_candidate("/tmp/a b.png"),
            None,
            "unescaped space is two words"
        );
        assert_eq!(
            pasted_image_path_candidate("/tmp/a\\ b.png"),
            Some(PathBuf::from("/tmp/a b.png"))
        );
        assert_eq!(
            pasted_image_path_candidate("file:///tmp/a%20b.png"),
            Some(PathBuf::from("/tmp/a b.png"))
        );
        assert_eq!(
            pasted_image_path_candidate("\"/tmp/a b.png\""),
            Some(PathBuf::from("/tmp/a b.png"))
        );
        assert_eq!(
            pasted_image_path_candidate("~/shot.png"),
            dirs::home_dir().map(|h| h.join("shot.png"))
        );
        assert_eq!(pasted_image_path_candidate("https://x/y.png"), None);
        assert_eq!(pasted_image_path_candidate("relative/dir"), None);
        assert_eq!(pasted_image_path_candidate("a\nb.png"), None);
    }

    #[test]
    fn image_from_clipboard_text_reads_an_image_file() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
        let path = std::env::temp_dir().join("may-clipboard-text-png.png");
        std::fs::write(&path, png).expect("write temp png");
        let image = image_from_clipboard_text(path.to_str().expect("utf-8 path"))
            .expect("read")
            .expect("image");
        assert_eq!(image.format, ImageFormat::Png);
        assert_eq!(image.data, png);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn image_from_clipboard_text_ignores_plain_text_and_missing_files() {
        assert!(image_from_clipboard_text("hello").unwrap().is_none());
        assert!(
            image_from_clipboard_text("/tmp/may-clipboard-missing-nope.png")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn format_detection_uses_magic_bytes() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let jpg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F'];
        assert_eq!(ImageFormat::from_magic_bytes(&png), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::from_magic_bytes(&jpg), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::from_magic_bytes(b"not-an-image"), None);
    }
}

/// Manager for pasted text content.
#[derive(Debug)]
pub struct TextManager {
    session_dir: PathBuf,
    counter: u32,
}

impl TextManager {
    /// Create a new text manager.
    #[must_use]
    pub fn new(session_dir: &Path) -> Self {
        Self {
            session_dir: session_dir.join("pastes"),
            counter: 0,
        }
    }

    /// Save large text to a file and return the path.
    ///
    /// # Errors
    ///
    /// Returns [`ImageError::Io`] when creating the paste directory or writing
    /// the file fails.
    pub async fn save(&mut self, text: &str) -> Result<PathBuf, ImageError> {
        async_fs::create_dir_all(&self.session_dir).await?;
        self.counter += 1;
        let filename = format!("paste_{:04}.txt", self.counter);
        let path = self.session_dir.join(filename);
        async_fs::write(&path, text).await?;
        Ok(path)
    }
}

/// Convert basic markdown to simple HTML for rich clipboard copy.
#[must_use]
pub fn markdown_to_basic_html(markdown: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, html};

    // Raw HTML in the text is shown as text, never passed through.
    let events = Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    )
    .map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

/// Try to write plain text into terminal clipboard via OSC52.
///
/// # Errors
///
/// Returns [`ImageError::Io`] when writing the escape sequence to standard
/// output fails.
pub fn copy_via_osc52(text: &str) -> Result<(), ImageError> {
    let payload = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut stdout = std::io::stdout();
    stdout
        .write_all(format!("\x1b]52;c;{payload}\x07").as_bytes())
        .map_err(ImageError::Io)?;
    stdout.flush().map_err(ImageError::Io)
}

/// Copy both rich text (HTML + alt text) and plain text using arboard.
///
/// # Errors
///
/// Returns [`ImageError::Clipboard`] when the clipboard cannot be opened or
/// the write fails.
pub fn set_clipboard_html(html: &str, alt_text: &str) -> Result<(), ImageError> {
    let mut clipboard = Clipboard::new().map_err(|e| ImageError::Clipboard(e.to_string()))?;
    clipboard
        .set_html(html, Some(alt_text))
        .map_err(|e| ImageError::Clipboard(e.to_string()))
}
