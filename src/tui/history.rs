//! Input history management for terminal command recall.

use std::fs;
use std::path::{Path, PathBuf};

/// Maximum number of history entries to keep.
const MAX_HISTORY_SIZE: usize = 1000;

/// Input history with navigation.
pub struct InputHistory {
    /// History entries (oldest first).
    entries: Vec<String>,
    /// Current navigation index (None = not navigating, editing new input).
    nav_index: Option<usize>,
    /// Saved current input when navigating.
    saved_input: String,
    /// Path to history file.
    history_file: PathBuf,
}

impl InputHistory {
    /// Create a new history, loading from file if exists.
    #[must_use] 
    pub fn new(may_dir: &Path) -> Self {
        let history_file = may_dir.join("history");
        let entries = Self::load_from_file(&history_file);

        Self {
            entries,
            nav_index: None,
            saved_input: String::new(),
            history_file,
        }
    }

    /// Load history from file.
    fn load_from_file(path: &Path) -> Vec<String> {
        match fs::read_to_string(path) {
            Ok(content) => content
                .lines()
                .filter(|line| !line.is_empty())
                .map(String::from)
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Save history to file.
    pub fn save(&self) {
        if let Some(parent) = self.history_file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let content = self.entries.join("\n");
        let _ = fs::write(&self.history_file, content);
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
    pub fn next(&mut self) -> Option<&str> {
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
