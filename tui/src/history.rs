//! Input history management for terminal command recall.

use std::path::{Path, PathBuf};

use async_channel::{Receiver, Sender};

/// Maximum number of history entries to keep.
const MAX_HISTORY_SIZE: usize = 1000;

/// Serialize one history entry as a JSON string on a single line; entries
/// hold their newlines inside the string's escapes, so multi-line prompts
/// survive a save/reload round trip. A line from an older file that is not
/// a JSON string loads as the literal text it already is.
fn encode_entry(entry: &str) -> String {
    serde_json::to_string(entry).unwrap_or_else(|_| entry.to_string())
}

/// The inverse of [`encode_entry`]: a JSON string decodes to its content,
/// anything else is the literal entry a pre-escaping file wrote.
fn decode_entry(line: &str) -> String {
    serde_json::from_str::<String>(line).unwrap_or_else(|_| line.to_string())
}

/// Serialize the entries for the history file: one JSON string per line.
fn encode_entries(entries: &[String]) -> String {
    entries
        .iter()
        .map(|entry| encode_entry(entry))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the history file: each line is an entry, JSON-decoded when it
/// was written by [`encode_entries`].
fn decode_entries(content: &str) -> Vec<String> {
    content
        .lines()
        .filter(|line| !line.is_empty())
        .map(decode_entry)
        .collect()
}

/// A history file's entries, read off the UI loop on a plain thread — the
/// read is blocking file I/O, so a thread needs no executor the TUI's
/// tests would have to install; the receiver delivers the entries once.
#[allow(
    clippy::disallowed_methods,
    reason = "runs on a dedicated background thread"
)]
fn load_from_file(path: PathBuf) -> Receiver<Vec<String>> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let entries = std::fs::read_to_string(&path)
            .map_or_else(|_| Vec::new(), |content| decode_entries(&content));
        let _ = tx.try_send(entries);
    });
    rx
}

/// The single writer draining save requests for `path`: each request is
/// the whole file content, so pending ones collapse to the newest before
/// the write runs.
#[allow(
    clippy::disallowed_methods,
    reason = "runs on a dedicated writer thread"
)]
fn spawn_writer(path: PathBuf) -> Sender<String> {
    let (tx, rx) = async_channel::unbounded::<String>();
    std::thread::spawn(move || {
        while let Ok(content) = rx.recv_blocking() {
            let mut content = content;
            while let Ok(newer) = rx.try_recv() {
                content = newer;
            }
            if let Some(parent) = path.parent()
                && let Err(error) = std::fs::create_dir_all(parent)
            {
                tracing::warn!(%error, "failed to create the history directory");
                continue;
            }
            if let Err(error) = std::fs::write(&path, &content) {
                tracing::warn!(%error, "failed to write the input history");
            }
        }
    });
    tx
}

/// Input history with navigation.
#[derive(Debug)]
pub struct InputHistory {
    /// History entries (oldest first).
    entries: Vec<String>,
    /// Current navigation index (None = not navigating, editing new input).
    nav_index: Option<usize>,
    /// Saved current input when navigating.
    saved_input: String,
    /// The load task's answer, until it arrives.
    pending_load: Option<Receiver<Vec<String>>>,
    /// Writer task's inbox; `None` until the first save.
    writer: Option<Sender<String>>,
    /// Path to history file.
    history_file: PathBuf,
}

impl InputHistory {
    /// Create a new history; the file's entries load off the UI loop and
    /// land through [`Self::poll_load`].
    #[must_use]
    pub fn new(may_dir: &Path) -> Self {
        let history_file = may_dir.join("history");
        Self {
            entries: Vec::new(),
            nav_index: None,
            saved_input: String::new(),
            pending_load: Some(load_from_file(history_file.clone())),
            writer: None,
            history_file,
        }
    }

    /// An empty history that writes nothing — for tests and the CLI paths
    /// with no `~/.may`.
    #[cfg(test)]
    const fn empty_for_test() -> Self {
        Self {
            entries: Vec::new(),
            nav_index: None,
            saved_input: String::new(),
            pending_load: None,
            writer: None,
            history_file: PathBuf::new(),
        }
    }

    /// Apply the loaded entries when they arrive. Entries the user pushed
    /// while the read ran are newer, so they stay after the loaded ones.
    pub fn poll_load(&mut self) {
        let Some(pending) = &self.pending_load else {
            return;
        };
        let Ok(loaded) = pending.try_recv() else {
            if pending.is_closed() {
                self.pending_load = None;
            }
            return;
        };
        self.pending_load = None;
        if self.entries.is_empty() {
            self.entries = loaded;
            return;
        }
        // A push during the load duplicated the file's newest entry when
        // the user resent it: the boundary dedupes exactly that overlap.
        let mut merged = loaded;
        for entry in std::mem::take(&mut self.entries) {
            if merged.last() != Some(&entry) {
                merged.push(entry);
            }
        }
        if merged.len() > MAX_HISTORY_SIZE {
            merged.drain(0..merged.len() - MAX_HISTORY_SIZE);
        }
        self.entries = merged;
    }

    /// Save history to file — serialized here, written by the writer task.
    pub fn save(&mut self) {
        let writer = self
            .writer
            .get_or_insert_with(|| spawn_writer(self.history_file.clone()));
        let _ = writer.try_send(encode_entries(&self.entries));
    }

    /// Add an entry to history.
    pub fn push(&mut self, input: String) {
        // Don't add empty or duplicate consecutive entries
        if input.is_empty() {
            return;
        }
        if self.entries.last() == Some(&input) {
            return;
        }

        self.entries.push(input);

        // Trim to max size
        if self.entries.len() > MAX_HISTORY_SIZE {
            let excess = self.entries.len() - MAX_HISTORY_SIZE;
            self.entries.drain(0..excess);
        }

        // Reset navigation
        self.nav_index = None;
        self.saved_input.clear();
    }

    /// Navigate to previous entry (Up arrow).
    /// Returns the entry to display, or None if at the beginning.
    pub fn prev(&mut self, current_input: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }

        match self.nav_index {
            None => {
                // Start navigating from the end
                self.saved_input = current_input.to_string();
                let idx = self.entries.len() - 1;
                self.nav_index = Some(idx);
                Some(&self.entries[idx])
            }
            Some(idx) if idx > 0 => {
                // Move backwards
                let new_idx = idx - 1;
                self.nav_index = Some(new_idx);
                Some(&self.entries[new_idx])
            }
            Some(_) => {
                // Already at the beginning
                self.nav_index.map(|i| self.entries[i].as_str())
            }
        }
    }

    /// Navigate to next entry (Down arrow).
    /// Returns the entry to display, or None if returning to current input.
    pub fn next_entry(&mut self) -> Option<&str> {
        match self.nav_index {
            None => None,
            Some(idx) => {
                if idx + 1 < self.entries.len() {
                    // Move forward in history
                    let new_idx = idx + 1;
                    self.nav_index = Some(new_idx);
                    Some(&self.entries[new_idx])
                } else {
                    // Return to saved input
                    self.nav_index = None;
                    Some(&self.saved_input)
                }
            }
        }
    }

    /// Reset navigation (e.g., when user types).
    pub fn reset_navigation(&mut self) {
        self.nav_index = None;
        self.saved_input.clear();
    }

    /// Check if currently navigating history.
    #[must_use]
    pub const fn is_navigating(&self) -> bool {
        self.nav_index.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_line_entries_survive_a_save_reload_round_trip() {
        let entries = vec![
            "one line".to_string(),
            "你好\n世界\nこんにちは\n안녕하세요".to_string(),
            "a \"quoted\" entry with \\ backslash".to_string(),
        ];
        let content = encode_entries(&entries);
        assert_eq!(decode_entries(&content), entries);
    }

    #[test]
    fn a_legacy_plain_line_file_still_loads() {
        let content = "first\nsecond with 你好 emoji 🎉\n";
        assert_eq!(
            decode_entries(content),
            vec!["first".to_string(), "second with 你好 emoji 🎉".to_string()]
        );
    }

    #[test]
    fn a_late_load_merges_behind_newer_pushes() {
        let mut history = InputHistory::empty_for_test();
        history.push("pushed during load".to_string());
        let (tx, rx) = async_channel::bounded(1);
        tx.try_send(vec!["older one".to_string(), "older two".to_string()])
            .expect("send");
        history.pending_load = Some(rx);

        history.poll_load();

        assert_eq!(
            history.entries,
            vec![
                "older one".to_string(),
                "older two".to_string(),
                "pushed during load".to_string(),
            ]
        );
    }

    #[test]
    fn a_load_that_duplicates_the_last_push_dedupes_the_boundary() {
        let mut history = InputHistory::empty_for_test();
        history.push("same".to_string());
        let (tx, rx) = async_channel::bounded(1);
        tx.try_send(vec!["a".to_string(), "same".to_string()])
            .expect("send");
        history.pending_load = Some(rx);

        history.poll_load();

        assert_eq!(history.entries, vec!["a".to_string(), "same".to_string()]);
    }
}
