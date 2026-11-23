//! The per-tool-call store of images an IPC command produced.
//!
//! A command that writes a screenshot records it here; the call's hooks take
//! the records when the tool result becomes a `Message::Tool`, so the model
//! receives the image and, on text-dense ones, the recognized text.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aither_core::llm::ToolCallId;
use serde::{Deserialize, Serialize};

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
            guard
                .by_call
                .entry(call.clone())
                .or_default()
                .extend(images);
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
        assert_eq!(store.take(&call), Vec::<RecordedCallImage>::new());

        // Late recording after scope ended is dropped rather than leaking.
        store.record(&call, vec![img]);
        assert_eq!(store.take(&call), Vec::<RecordedCallImage>::new());
    }
}
