//! Terminal image rendering support.
//!
//! This module provides image rendering in terminals that support native graphics protocols
//! (Kitty, iTerm2, Sixel). If native protocols are not supported, images are displayed as links.
//!
//! The Picker must be initialized before entering raw mode and is stored in App state.

use anyhow;
use image::DynamicImage;
use ratatui::prelude::*;
use ratatui_image::StatefulImage;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;

/// Terminal image support state.
/// Created once at startup, before raw mode, and stored in App.
/// Only created if terminal supports native graphics protocols (Kitty, iTerm2, Sixel).
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
    pub fn protocol(&self) -> ProtocolType {
        self.protocol
    }

    /// Get a reference to the picker.
    pub fn picker(&self) -> &Picker {
        &self.picker
    }

    /// Create an ImageState from image data.
    pub fn create_image_state(&self, image: DynamicImage) -> ImageState {
        let protocol = self.picker.new_resize_protocol(image);
        ImageState { protocol }
    }

    /// Create an ImageState from raw bytes.
    pub fn create_image_state_from_bytes(&self, data: &[u8]) -> Option<ImageState> {
        let image = image::load_from_memory(data).ok()?;
        Some(self.create_image_state(image))
    }
}

/// Image state for rendering.
/// Created via TerminalImageSupport.
pub struct ImageState {
    /// The protocol state for rendering
    pub protocol: StatefulProtocol,
}

/// Render an image using the stateful protocol.
pub fn render_image(frame: &mut Frame, area: Rect, state: &mut ImageState) {
    let image_widget = StatefulImage::default();
    frame.render_stateful_widget(image_widget, area, &mut state.protocol);
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
        let path = if url.starts_with("file://") {
            &url[7..]
        } else {
            url
        };
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
async fn fetch_image_http(url: &str) -> anyhow::Result<Vec<u8>> {
    use zenwave::{Client, client, header};

    let mut backend = client();
    let bytes = backend
        .get(url)?
        .header(
            header::USER_AGENT.as_str(),
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        )?
        .header(header::ACCEPT.as_str(), "image/*,*/*")?
        .bytes()
        .await?;

    Ok(bytes.to_vec())
}

/// Render image placeholder when actual rendering is not possible.
pub fn render_placeholder(alt: &str, url: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("🖼 ", Style::default()),
            Span::styled(
                alt.to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" <{}>", url), Style::default().fg(Color::DarkGray)),
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
}
