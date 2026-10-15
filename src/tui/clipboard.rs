//! Clipboard handling for image paste.

use std::io::Write;
use std::path::{Path, PathBuf};

use arboard::Clipboard;
use async_fs;
use base64::Engine;
use image::ImageFormat as ImgFormat;
use url::Url;
use zenwave::ResponseExt;

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

/// Try to get an image from the clipboard.
///
/// # Errors
///
/// Returns [`ImageError::Clipboard`] when the clipboard cannot be opened or
/// read, or [`ImageError::Encoding`] when the image cannot be encoded as PNG
/// (including dimensions that do not fit into `u32`).
pub fn get_clipboard_image() -> Result<Option<ClipboardImage>, ImageError> {
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

    // Encode as PNG
    let mut png_data = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut png_data);
    img.write_to(&mut cursor, ImgFormat::Png)
        .map_err(|e| ImageError::Encoding(e.to_string()))?;

    Ok(Some(ClipboardImage {
        data: png_data,
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

/// Try to read from the clipboard, preferring images over text.
///
/// # Errors
///
/// Returns [`ImageError`] when reading the clipboard image or text fails.
pub fn read_clipboard() -> Result<ClipboardPasteResult, ImageError> {
    if let Some(image) = get_clipboard_image()? {
        return Ok(ClipboardPasteResult::image(image));
    }

    if let Some(text) = get_clipboard_text()? {
        return Ok(ClipboardPasteResult::text(text));
    }

    Ok(ClipboardPasteResult::empty())
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
        .to_string();

    let data = response
        .into_bytes()
        .await
        .map_err(|e| ImageError::Download(e.to_string()))?;

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

    Ok(ClipboardImage {
        data: data.to_vec(),
        format,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    // Keep this intentionally simple/safe for clipboard usage.
    let escaped = markdown
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");

    let mut html = escaped.replace("\n\n", "</p><p>");
    html = html.replace('\n', "<br />");
    html = html.replace("**", "__BOLD_TOGGLE__");
    html = html.replace('*', "__ITALIC_TOGGLE__");

    // Toggle-style replacement: good enough for LLM markdown snippets.
    let mut out = String::new();
    let mut bold = false;
    let mut i = 0usize;
    while let Some(pos) = html[i..].find("__BOLD_TOGGLE__") {
        let abs = i + pos;
        out.push_str(&html[i..abs]);
        out.push_str(if bold { "</strong>" } else { "<strong>" });
        bold = !bold;
        i = abs + "__BOLD_TOGGLE__".len();
    }
    out.push_str(&html[i..]);

    let html = out;
    let mut out = String::new();
    let mut italic = false;
    let mut i = 0usize;
    while let Some(pos) = html[i..].find("__ITALIC_TOGGLE__") {
        let abs = i + pos;
        out.push_str(&html[i..abs]);
        out.push_str(if italic { "</em>" } else { "<em>" });
        italic = !italic;
        i = abs + "__ITALIC_TOGGLE__".len();
    }
    out.push_str(&html[i..]);

    format!("<p>{out}</p>")
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
