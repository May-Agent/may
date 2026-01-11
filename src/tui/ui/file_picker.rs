//! File picker component with fuzzy search.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use nucleo::{Config as NucleoConfig, Nucleo};

/// File picker state.
pub struct FilePicker {
    /// Search query
    query: String,
    /// Matched files
    matches: Vec<PathBuf>,
    /// Currently selected index
    selected: usize,
    /// All files in base directory
    all_files: Vec<PathBuf>,
    /// Fuzzy matcher
    matcher: Nucleo<PathBuf>,
}

impl FilePicker {
    /// Create a new file picker rooted at the given directory.
    pub fn new(base_dir: &Path) -> Self {
        let all_files = collect_files(base_dir);

        // Initialize nucleo matcher
        let config = NucleoConfig::DEFAULT;
        let matcher = Nucleo::new(config, std::sync::Arc::new(|| {}), None, 1);

        // Add all files to the matcher
        let injector = matcher.injector();
        for file in &all_files {
            let _ = injector.push(file.clone(), |path, cols| {
                cols[0] = path.to_string_lossy().into();
            });
        }

        Self {
            query: String::new(),
            matches: all_files.clone(),
            selected: 0,
            all_files,
            matcher,
        }
    }

    /// Update the search query.
    pub fn update_query(&mut self, query: &str) {
        self.query = query.to_string();

        if query.is_empty() {
            self.matches = self.all_files.clone();
        } else {
            // Update pattern
            self.matcher.pattern.reparse(
                0,
                query,
                nucleo::pattern::CaseMatching::Smart,
                nucleo::pattern::Normalization::Smart,
                false,
            );

            // Tick the matcher to process
            self.matcher.tick(10);

            // Collect matches
            let snapshot = self.matcher.snapshot();
            self.matches = snapshot
                .matched_items(..)
                .map(|item| item.data.clone())
                .collect();
        }

        // Reset selection
        self.selected = 0;
    }

    /// Get current query.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Insert a character into the query.
    pub fn insert_char(&mut self, c: char) {
        self.query.push(c);
        self.update_query(&self.query.clone());
    }

    /// Delete last character from query.
    pub fn delete_char(&mut self) {
        self.query.pop();
        self.update_query(&self.query.clone());
    }

    /// Select next match.
    pub fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    /// Select previous match.
    pub fn select_prev(&mut self) {
        if !self.matches.is_empty() {
            self.selected = self.selected.checked_sub(1).unwrap_or(self.matches.len() - 1);
        }
    }

    /// Get the selected file path.
    pub fn selected_path(&self) -> Option<&PathBuf> {
        self.matches.get(self.selected)
    }

    /// Confirm selection and return the selected path.
    pub fn confirm(&self) -> Option<PathBuf> {
        self.selected_path().cloned()
    }

    /// Get visible matches for rendering.
    pub fn visible_matches(&self, max_count: usize) -> impl Iterator<Item = (usize, &PathBuf)> {
        let start = self.selected.saturating_sub(max_count / 2);
        self.matches
            .iter()
            .enumerate()
            .skip(start)
            .take(max_count)
    }

    /// Get the selected index.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Get total match count.
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }
}

/// Collect all files from directory, respecting gitignore.
fn collect_files(base_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    let walker = WalkBuilder::new(base_dir)
        .hidden(true)        // Skip hidden files/directories (including .git)
        .ignore(true)        // Respect .ignore files
        .git_ignore(true)    // Respect .gitignore
        .git_global(true)    // Respect global gitignore
        .git_exclude(true)   // Respect .git/info/exclude
        .max_depth(Some(10)) // Limit depth
        .build();

    for entry in walker.flatten() {
        if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            files.push(entry.into_path());
        }
    }

    files.sort();
    files
}
