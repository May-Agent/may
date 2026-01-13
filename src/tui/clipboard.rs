//! Clipboard handling for image paste.

use std::fs;
use std::path::{Path, PathBuf};

use arboard::Clipboard;
use image::ImageFormat as ImgFormat;

/// Image format.
#[derive(Debug, Clone, Copy)]
pub enum ImageFormat {
    Png,
    Jpeg,
}

impl ImageFormat {
    /// Get file extension.
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
        }
    }
}

/// A clipboard image.
#[derive(Debug, Clone)]
pub struct ClipboardImage {
    pub data: Vec<u8>,
    pub format: ImageFormat,
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
    pub fn save(&mut self, image: &ClipboardImage) -> Result<PathBuf, ImageError> {
        // Ensure directory exists
        fs::create_dir_all(&self.session_images_dir)?;

        let path = self.next_path(image.format);
        fs::write(&path, &image.data)?;

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

/// Copy text to the system clipboard.
pub fn set_clipboard_text(text: &str) -> Result<(), ImageError> {
    let mut clipboard = Clipboard::new().map_err(|e| ImageError::Clipboard(e.to_string()))?;
    clipboard
        .set_text(text)
        .map_err(|e| ImageError::Clipboard(e.to_string()))
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
    pub fn save(&mut self, text: &str) -> Result<PathBuf, ImageError> {
        fs::create_dir_all(&self.session_dir)?;
        self.counter += 1;
        let filename = format!("paste_{:04}.txt", self.counter);
        let path = self.session_dir.join(filename);
        fs::write(&path, text)?;
        Ok(path)
    }
}
