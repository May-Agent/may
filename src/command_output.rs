//! Host-side text for the output streams of a command run.

use aither_core::llm::CommandStream;

/// Text a host shows for a stream: inline output as is, stored and image
/// output as the view that names the stored file.
#[must_use]
pub fn stream_text(stream: &CommandStream) -> String {
    match stream {
        CommandStream::Inline(text) => text.clone(),
        CommandStream::Stored { .. } | CommandStream::Image { .. } => stream.render_for_model(),
    }
}
