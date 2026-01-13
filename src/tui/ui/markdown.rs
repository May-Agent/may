//! Markdown rendering with streaming support.
//!
//! This module provides markdown rendering optimized for LLM streaming output.
//! It uses a custom streaming markdown parser that caches rendered blocks.

use ratatui::prelude::*;

use super::streaming_markdown::StreamingMarkdown;

/// Result of rendering markdown with image position tracking.
pub struct MarkdownRenderResult {
    /// Rendered lines
    pub lines: Vec<Line<'static>>,
    /// Image positions: (url, line_index) for each image
    pub image_positions: Vec<(String, usize)>,
}

/// Render markdown text to ratatui lines.
///
/// This is a convenience wrapper that creates a temporary StreamingMarkdown
/// renderer for one-shot rendering. For streaming use cases, prefer using
/// StreamingMarkdown directly.
///
/// `images_supported` controls how images are rendered:
/// - true: show loading indicator (actual image rendered as overlay)
/// - false: show as styled link
pub fn render_markdown_with_config(text: &str, images_supported: bool) -> Vec<Line<'static>> {
    let mut renderer = StreamingMarkdown::with_image_support(images_supported);
    renderer.push(text);
    renderer.flush(); // Ensure last line is parsed even without trailing newline
    renderer.lines().to_vec()
}

/// Render markdown and return both lines and image positions.
///
/// Use this when you need to know where images are located in the rendered output
/// for rendering actual images as overlays.
pub fn render_markdown_with_positions(text: &str, images_supported: bool) -> MarkdownRenderResult {
    let mut renderer = StreamingMarkdown::with_image_support(images_supported);
    renderer.push(text);
    renderer.flush(); // Ensure last line is parsed even without trailing newline
    MarkdownRenderResult {
        lines: renderer.lines().to_vec(),
        image_positions: renderer.image_positions().to_vec(),
    }
}

/// Render markdown text to ratatui lines (no image support).
/// For image support, use `render_markdown_with_config`.
pub fn render_markdown(text: &str) -> Vec<Line<'static>> {
    render_markdown_with_config(text, false)
}

/// Extract image URLs from markdown text.
///
/// Returns a list of (alt_text, url) pairs for all images found.
pub fn extract_images(text: &str) -> Vec<(String, String)> {
    let mut renderer = StreamingMarkdown::new();
    renderer.push(text);
    renderer.flush(); // Ensure last line is parsed even without trailing newline
    renderer.images()
}
