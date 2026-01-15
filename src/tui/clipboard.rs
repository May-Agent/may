//! Clipboard handling for image paste.

use std::path::{Path, PathBuf};

use async_fs;
use arboard::Clipboard;
use image::ImageFormat as ImgFormat;
use url::Url;
use zenwave::ResponseExt;

/// Image format.
#[derive(Debug, Clone, Copy)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageFormat {
    /// Get file extension.
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }

    /// Detect format from URL or content-type.
    pub fn from_url(url: &str) -> Option<Self> {
        let lower = url.to_lowercase();
        // Check file extension in URL path
        if lower.contains(".png") {
            Some(Self::Png)
        } else if lower.contains(".jpg") || lower.contains(".jpeg") {
            Some(Self::Jpeg)
        } else if lower.contains(".gif") {
            Some(Self::Gif)
        } else if lower.contains(".webp") {
            Some(Self::Webp)
        } else {
            None
        }
    }

    /// Detect format from content-type header.
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
}

/// A clipboard image.
#[derive(Debug, Clone)]
pub struct ClipboardImage {
    pub data: Vec<u8>,
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
    pub fn image(image: ClipboardImage) -> Self {
        Self {
            image: Some(image),
            text: None,
        }
    }

    /// Create a result containing text.
    #[must_use]
    pub fn text(text: String) -> Self {
        Self {
            image: None,
            text: Some(text),
        }
    }

    /// Create an empty result.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            image: None,
            text: None,
        }
    }

    /// Get the image, if present.
    #[must_use]
    pub fn image_ref(&self) -> Option<&ClipboardImage> {
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
    pub fn is_empty(&self) -> bool {
        self.image.is_none() && self.text.is_none()
    }
}

/// Image manager for handling pasted images.
pub struct ImageManager {
    session_images_dir: PathBuf,
    counter: u32,
}

impl ImageManager {
    /// Create a new image manager.
    pub fn new(session_dir: &Path) -> Self {
        Self {
            session_images_dir: session_dir.join("images"),
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
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Clipboard error: {0}")]
    Clipboard(String),
    #[error("Image encoding error: {0}")]
    Encoding(String),
    #[error("Download error: {0}")]
    Download(String),
}

/// Try to get an image from the clipboard.
pub fn get_clipboard_image() -> Result<Option<ClipboardImage>, ImageError> {
    let mut clipboard =
        Clipboard::new().map_err(|e| ImageError::Clipboard(e.to_string()))?;

    // Try to get image from clipboard
    let image_data = match clipboard.get_image() {
        Ok(img) => img,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(e) => return Err(ImageError::Clipboard(e.to_string())),
    };

    // Convert to PNG bytes
    let width = image_data.width as u32;
    let height = image_data.height as u32;

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
pub fn get_clipboard_text() -> Result<Option<String>, ImageError> {
    let mut clipboard = Clipboard::new().map_err(|e| ImageError::Clipboard(e.to_string()))?;

    match clipboard.get_text() {
        Ok(text) if !text.is_empty() => Ok(Some(text)),
        Ok(_) => Ok(None),
        Err(arboard::Error::ContentNotAvailable) => Ok(None),
        Err(e) => Err(ImageError::Clipboard(e.to_string())),
    }
}

/// Try to read from the clipboard, preferring images over text.
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
pub fn set_clipboard_text(text: &str) -> Result<(), ImageError> {
    let mut clipboard = Clipboard::new().map_err(|e| ImageError::Clipboard(e.to_string()))?;
    clipboard
        .set_text(text)
        .map_err(|e| ImageError::Clipboard(e.to_string()))
}

/// Check if text is an image URL.
pub fn is_image_url(text: &str) -> Option<Url> {
    let trimmed = text.trim();

    // Must be a valid URL
    let url = Url::parse(trimmed).ok()?;

    // Must be http or https
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }

    // Check if the URL looks like an image
    if ImageFormat::from_url(url.as_str()).is_some() {
        return Some(url);
    }

    // Check for common image hosting patterns
    let host = url.host_str().unwrap_or_default();
    let path = url.path().to_lowercase();

    // Known image hosting services that may not have file extensions
    let is_image_host = host.contains("imgur.com")
        || host.contains("i.redd.it")
        || host.contains("pbs.twimg.com")
        || host.contains("cdn.discordapp.com")
        || host.contains("media.discordapp.net")
        || host.contains("images.unsplash.com")
        || host.contains("i.ibb.co")
        || host.contains("prnt.sc")
        || host.contains("gyazo.com")
        || host.contains("snipboard.io");

    // Check path for image indicators
    let has_image_path = path.contains("/image/")
        || path.contains("/images/")
        || path.contains("/photo/")
        || path.contains("/photos/")
        || path.contains("/media/");

    if is_image_host || has_image_path {
        return Some(url);
    }

    None
}

/// Download an image from a URL.
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

    // Try to detect format from Content-Type header
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    let format = ImageFormat::from_content_type(content_type)
        .or_else(|| ImageFormat::from_url(url.as_str()))
        .unwrap_or(ImageFormat::Png);

    let data = response
        .into_bytes()
        .await
        .map_err(|e| ImageError::Download(e.to_string()))?;

    Ok(ClipboardImage {
        data: data.to_vec(),
        format,
    })
}

/// Manager for pasted text content.
pub struct TextManager {
    session_dir: PathBuf,
    counter: u32,
}

impl TextManager {
    /// Create a new text manager.
    pub fn new(session_dir: &Path) -> Self {
        Self {
            session_dir: session_dir.join("pastes"),
            counter: 0,
        }
    }

    /// Save large text to a file and return the path.
    pub async fn save(&mut self, text: &str) -> Result<PathBuf, ImageError> {
        async_fs::create_dir_all(&self.session_dir).await?;
        self.counter += 1;
        let filename = format!("paste_{:04}.txt", self.counter);
        let path = self.session_dir.join(filename);
        async_fs::write(&path, text).await?;
        Ok(path)
    }
}
