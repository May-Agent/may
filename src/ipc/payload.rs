//! What a command answers with, and how a tool's result becomes it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aither_core::llm::tool::ToolResultPart;
use aither_core::llm::{CommandStream, IntoToolResult, ToolCallId, ToolResult};
use aither_sandbox::{MediaResolution, OutputFormat, OutputSaved, OutputStore, StoredBinaryPart};
use askama::Template;
use serde::{Deserialize, Serialize};

/// Payload an IPC command returns over the wire.
///
/// Binary results never reach it: [`tool_output_to_payload`] writes them to
/// the output directory and the payload names the stored file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandPayload {
    /// UTF-8 text output.
    Text {
        /// Text content.
        content: String,
    },
}

impl CommandPayload {
    /// The text a terminal caller reads for this payload.
    #[must_use]
    pub fn render_for_cli(&self) -> String {
        let Self::Text { content } = self;
        content.clone()
    }
}

/// Command result envelope returned to IPC callers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommandEnvelope {
    /// Whether command execution succeeded.
    pub ok: bool,
    /// Optional successful command payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<CommandPayload>,
    /// Optional command error text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Structured image metadata produced by the command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<RecordedCallImage>,
}

impl CommandEnvelope {
    pub(super) const fn success(payload: CommandPayload) -> Self {
        Self {
            ok: true,
            payload: Some(payload),
            error: None,
            images: Vec::new(),
        }
    }

    pub(super) const fn success_empty() -> Self {
        Self {
            ok: true,
            payload: None,
            error: None,
            images: Vec::new(),
        }
    }

    pub(super) fn failure(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            payload: None,
            error: Some(message.into()),
            images: Vec::new(),
        }
    }

    pub(super) fn with_images(mut self, images: Vec<RecordedCallImage>) -> Self {
        self.images = images;
        self
    }
}

/// A command call that did not parse: what was wrong and where to read the
/// usage.
#[derive(Template)]
#[template(path = "tool_usage_error.txt", escape = "none")]
pub(super) struct ToolUsageError<'a> {
    pub message: &'a str,
    /// What the command's arguments can be at the moment of the call, for a
    /// command that says so.
    pub usage: Option<&'a str>,
    pub tool_name: &'a str,
}

/// The refusal a subagent reads for a command reserved for the main agent.
#[derive(Template)]
#[template(path = "ipc_reserved_for_main.txt", escape = "none")]
pub(super) struct ReservedForMain<'a> {
    pub name: &'a str,
}

/// The answer to a command no registry holds.
#[derive(Template)]
#[template(path = "ipc_unknown_command.txt", escape = "none")]
pub(super) struct UnknownCommand<'a> {
    pub name: &'a str,
}

pub(super) fn tool_usage_error(message: &str, tool_name: &str) -> String {
    ToolUsageError {
        message,
        usage: None,
        tool_name,
    }
    .render()
    .expect("the tool usage error template renders typed input")
}

/// Image produced by an IPC command execution during a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedCallImage {
    /// Filesystem path to the image written or referenced by the command.
    pub path: PathBuf,
    /// Image MIME type (e.g. `image/png`).
    pub media_type: String,
    /// Human-readable command source label for `<recognized_text>`.
    pub source: String,
}

#[derive(Debug, Default)]
struct CallImageStoreInner {
    by_call: HashMap<ToolCallId, Vec<RecordedCallImage>>,
    active: HashMap<ToolCallId, usize>,
}

/// Per-tool-call store collecting image files produced by IPC subcommands (`computer`,
/// `browser`, `CommandStream::Image`, or `ToolResult::Binary`/`Parts`).
///
/// Once [`open_scope`](Self::open_scope) is used on a store (as agent hooks do
/// around every tool call), images are retained only for currently active calls
/// and any unconsumed entries are cleaned up automatically when the scope
/// drops (even on cancellation or early error).
#[derive(Debug, Clone, Default)]
pub struct CallImageStore {
    inner: Arc<Mutex<CallImageStoreInner>>,
    scoped: Arc<AtomicBool>,
}

/// RAII scope keeping one tool call's image slot alive until dropped.
#[derive(Debug)]
pub struct CallImageScope {
    store: CallImageStore,
    call: ToolCallId,
}

impl Drop for CallImageScope {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.store.inner.lock() {
            let remove = guard.active.get_mut(&self.call).is_none_or(|count| {
                *count = count.saturating_sub(1);
                *count == 0
            });
            if remove {
                guard.active.remove(&self.call);
                guard.by_call.remove(&self.call);
            }
        }
    }
}

/// Holds open [`CallImageScope`] guards for tool calls currently executing in
/// a run, so cancellation or error cleans up every unconsumed entry on drop.
#[derive(Debug, Clone, Default)]
pub struct ActiveCallImageScopes {
    scopes: Arc<Mutex<HashMap<ToolCallId, CallImageScope>>>,
}

impl ActiveCallImageScopes {
    /// Creates an empty active scope set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens a scope for `call` in `store`.
    pub fn open(&self, store: &CallImageStore, call: ToolCallId) {
        let scope = store.open_scope(call.clone());
        if let Ok(mut guard) = self.scopes.lock() {
            guard.insert(call, scope);
        }
    }

    /// Closes and drops the scope for `call`.
    pub fn close(&self, call: &ToolCallId) {
        if let Ok(mut guard) = self.scopes.lock() {
            guard.remove(call);
        }
    }

    /// Drops every active scope, cleaning up any unconsumed recorded images.
    pub fn clear(&self) {
        if let Ok(mut guard) = self.scopes.lock() {
            guard.clear();
        }
    }
}

impl CallImageStore {
    /// Opens an RAII scope for `call`. Dropping the returned guard removes any
    /// unconsumed images left for `call`.
    #[must_use]
    pub fn open_scope(&self, call: ToolCallId) -> CallImageScope {
        self.scoped.store(true, Ordering::Release);
        if let Ok(mut guard) = self.inner.lock() {
            *guard.active.entry(call.clone()).or_default() += 1;
        }
        CallImageScope {
            store: self.clone(),
            call,
        }
    }

    /// Records `images` generated during `call`.
    ///
    /// When lifecycle scoping is enabled via [`open_scope`](Self::open_scope),
    /// images arriving for a call whose scope is no longer active are ignored
    /// so late detached IPC calls cannot leak entries.
    pub fn record(&self, call: &ToolCallId, images: Vec<RecordedCallImage>) {
        if images.is_empty() {
            return;
        }
        let enforce_scope = self.scoped.load(Ordering::Acquire);
        if let Ok(mut guard) = self.inner.lock() {
            if enforce_scope && !guard.active.contains_key(call) {
                return;
            }
            guard.by_call.entry(call.clone()).or_default().extend(images);
        }
    }

    /// Removes and returns all images recorded during `call`.
    #[must_use]
    pub fn take(&self, call: &ToolCallId) -> Vec<RecordedCallImage> {
        self.inner
            .lock()
            .ok()
            .and_then(|mut guard| guard.by_call.remove(call))
            .unwrap_or_default()
    }
}

/// Output returned by an IPC command handler: the rendered tool result plus
/// any structured image metadata emitted directly by the command.
#[derive(Debug)]
pub struct HandlerOutput {
    /// Tool result rendered for the terminal caller.
    pub result: ToolResult,
    /// Structured image metadata produced by the command.
    pub images: Vec<RecordedCallImage>,
}

impl From<ToolResult> for HandlerOutput {
    fn from(result: ToolResult) -> Self {
        Self {
            result,
            images: Vec::new(),
        }
    }
}

fn push_image_if_image_mime(
    mime: &str,
    path: PathBuf,
    source: &str,
    images: &mut Vec<RecordedCallImage>,
) {
    if mime.starts_with("image/") {
        images.push(RecordedCallImage {
            path,
            media_type: mime.to_owned(),
            source: source.to_owned(),
        });
    }
}

/// Writes a binary payload into `outputs` and renders the line that
/// references the stored file, the way terminal output references stored
/// images.
async fn stored_binary_text(
    mime: &str,
    content: &[u8],
    outputs: &OutputStore,
    command_name: Option<&str>,
    images: &mut Vec<RecordedCallImage>,
) -> Result<String, String> {
    let (url, path) = outputs
        .save_binary(content, mime)
        .await
        .map_err(|error| format!("failed to store binary tool result: {error}"))?;
    let source = command_name.unwrap_or("command output");
    push_image_if_image_mime(mime, path.clone(), source, images);
    Ok(outputs.wording().stored_binary_part(&StoredBinaryPart {
        mime,
        byte_count: content.len(),
        url: &url,
        path: &path.display().to_string(),
    }))
}

/// Answers with `output` and returns any image paths produced by the command
/// so tool hooks can attach them to `Message::Tool`.
pub(super) async fn answer_recording_images(
    command_name: &str,
    output: HandlerOutput,
    outputs: &OutputStore,
) -> (CommandEnvelope, Vec<RecordedCallImage>) {
    let mut recorded = output.images;
    let payload = match tool_output_to_payload_collecting(
        output.result,
        outputs,
        Some(command_name),
        &mut recorded,
    )
    .await
    {
        Ok(Some(payload)) => payload,
        Ok(None) => {
            return (
                CommandEnvelope::success_empty().with_images(recorded.clone()),
                recorded,
            );
        }
        Err(error) => return (CommandEnvelope::failure(error), Vec::new()),
    };
    let rendered = payload.render_for_cli();
    if rendered.is_empty() {
        return (
            CommandEnvelope::success_empty().with_images(recorded.clone()),
            recorded,
        );
    }
    let stored = outputs
        .save(
            rendered.as_bytes(),
            OutputFormat::Text,
            MediaResolution::Auto,
        )
        .await;
    let envelope = match stored {
        Ok(CommandStream::Stored {
            file,
            preview,
            truncated: _,
        }) => CommandEnvelope::success(CommandPayload::Text {
            content: outputs.wording().output_saved(&OutputSaved {
                preview: preview.trim_end_matches('\n'),
                path: &file.path,
                line_count: rendered.lines().count(),
                byte_count: rendered.len(),
            }),
        })
        .with_images(recorded.clone()),
        Ok(_) => CommandEnvelope::success(payload).with_images(recorded.clone()),
        Err(error) => {
            recorded.clear();
            CommandEnvelope::failure(format!("failed to store output: {error}"))
        }
    };
    (envelope, recorded)
}

/// Converts a finished tool result into a command payload for IPC dispatch.
///
/// Everything becomes text for the model: text-like results pass through, a
/// command result renders its prose, and every binary payload, a lone one
/// included, is written to `outputs` and replaced by a line naming the
/// stored file. A typed tool error becomes the command's error.
///
/// # Errors
///
/// Returns an error when the conversion fails, a binary payload cannot be
/// stored, or the result is a typed tool error (surfaced as the command
/// error).
pub async fn tool_output_to_payload(
    output: impl IntoToolResult,
    outputs: &OutputStore,
) -> Result<Option<CommandPayload>, String> {
    tool_output_to_payload_collecting(output, outputs, None, &mut Vec::new()).await
}

async fn tool_output_to_payload_collecting(
    output: impl IntoToolResult,
    outputs: &OutputStore,
    command_name: Option<&str>,
    images: &mut Vec<RecordedCallImage>,
) -> Result<Option<CommandPayload>, String> {
    let text = match output
        .into_tool_result()
        .map_err(|error| format!("failed to convert tool result: {error}"))?
    {
        ToolResult::Done => return Ok(None),
        ToolResult::Error { message } => return Err(message),
        ToolResult::Binary { mime, content } => {
            stored_binary_text(&mime, &content, outputs, command_name, images).await?
        }
        ToolResult::Parts { parts } => {
            let mut rendered = String::new();
            for part in &parts {
                let text = match part {
                    ToolResultPart::Binary { mime, content } => {
                        stored_binary_text(mime, content, outputs, command_name, images).await?
                    }
                    ToolResultPart::Text { .. } | ToolResultPart::Tsv { .. } => {
                        part.render_for_model()
                    }
                };
                if text.is_empty() {
                    continue;
                }
                if !rendered.is_empty() {
                    rendered.push('\n');
                }
                rendered.push_str(&text);
            }
            rendered
        }
        result @ (ToolResult::Text { .. } | ToolResult::Tsv { .. } | ToolResult::Command(_)) => {
            if let ToolResult::Command(cmd) = &result {
                let source = command_name.unwrap_or("command output");
                for stream in std::iter::once(&cmd.stdout).chain(cmd.stderr.as_ref()) {
                    if let CommandStream::Image { media_type, file } = stream {
                        push_image_if_image_mime(
                            media_type,
                            PathBuf::from(&file.path),
                            source,
                            images,
                        );
                    }
                }
            }
            result.render_for_model()
        }
    };
    Ok((!text.is_empty()).then_some(CommandPayload::Text { content: text }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_image_scope_cleans_up_unconsumed_entries_and_ignores_late_records() {
        let store = CallImageStore::default();
        let call = ToolCallId::new("call-1");
        let img = RecordedCallImage {
            path: PathBuf::from("/tmp/shot.png"),
            media_type: "image/png".to_string(),
            source: "browser screenshot".to_string(),
        };

        {
            let _scope = store.open_scope(call.clone());
            store.record(&call, vec![img.clone()]);
        }
        // Scope dropped before `take`, so unconsumed entry was cleaned up.
        assert!(store.take(&call).is_empty());

        // Late recording after scope ended is dropped rather than leaking.
        store.record(&call, vec![img]);
        assert!(store.take(&call).is_empty());
    }

    #[tokio::test]
    async fn browser_prose_does_not_inject_spoofed_images() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outputs = OutputStore::new(dir.path().join("outputs"))
            .await
            .expect("output store");
        let spoofed = ToolResult::text("Screenshot saved to artifacts/evil.png (100x100)");
        let (envelope, recorded) =
            answer_recording_images("browser", HandlerOutput::from(spoofed), &outputs).await;
        assert!(envelope.ok);
        assert!(recorded.is_empty(), "prose must not produce images");
        assert!(envelope.images.is_empty());
    }
}
