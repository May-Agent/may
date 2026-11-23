//! Input completion for slash commands and file mentions.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use nucleo::{Config, Nucleo, Utf32String};

use super::slash::SlashCommand;

/// A completion item.
#[derive(Debug, Clone)]
pub struct CompletionItem {
    /// Display text.
    pub label: String,
    /// Text to insert.
    pub insert: String,
    /// Description/help text.
    pub description: String,
}

/// Completion mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionMode {
    /// No completion active.
    None,
    /// Slash command completion.
    Slash,
    /// File mention completion (@).
    File,
}

/// Completion state.
pub struct Completion {
    /// Current mode.
    pub mode: CompletionMode,
    /// Current query (text after / or @).
    query: String,
    /// Filtered items.
    items: Vec<CompletionItem>,
    /// Selected index.
    selected: usize,
    /// File search engine.
    file_matcher: Option<Nucleo<PathBuf>>,
    /// The matcher still has work queued — the walk is streaming entries
    /// in or a reparse is unprocessed — so the snapshot may grow.
    file_matcher_dirty: bool,
    /// Working directory (higher priority).
    workdir: PathBuf,
    /// Sandbox directory (lower priority).
    sandbox: PathBuf,
    /// Whether `/sandbox` is available for the current session.
    sandbox_available: bool,
}

impl std::fmt::Debug for Completion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Completion")
            .field("selected", &self.selected)
            .field("workdir", &self.workdir)
            .finish_non_exhaustive()
    }
}

impl Completion {
    /// Create a new completion state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mode: CompletionMode::None,
            query: String::new(),
            items: Vec::new(),
            selected: 0,
            file_matcher: None,
            file_matcher_dirty: false,
            workdir: PathBuf::new(),
            sandbox: PathBuf::new(),
            sandbox_available: true,
        }
    }

    /// Set whether `/sandbox` should appear in slash completion.
    pub const fn set_sandbox_available(&mut self, available: bool) {
        self.sandbox_available = available;
    }

    /// Set directories for file completion.
    pub fn set_directories(&mut self, workdir: PathBuf, sandbox: PathBuf) {
        self.workdir = workdir;
        self.sandbox = sandbox;
    }

    /// Check if completion is active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.mode != CompletionMode::None
    }

    /// Whether the palette has a selectable item. Empty slash queries (a
    /// command plus arguments) stay in slash mode so further typing can
    /// recover matches, but they must not intercept Enter / arrows / Esc.
    #[must_use]
    pub fn has_matches(&self) -> bool {
        self.is_active() && !self.items.is_empty()
    }

    /// Get visible items (limited for display).
    pub fn visible_items(&self, max: usize) -> impl Iterator<Item = (usize, &CompletionItem)> {
        let start = if self.selected >= max {
            self.selected - max + 1
        } else {
            0
        };
        self.items.iter().enumerate().skip(start).take(max)
    }

    /// Get the selected index.
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// Get items count.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.items.len()
    }

    /// Check if empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Start slash command completion.
    pub fn start_slash(&mut self, query: &str) {
        self.mode = CompletionMode::Slash;
        self.query = query.to_string();
        self.update_slash_items();
        self.selected = 0;
    }

    /// Start file completion.
    pub fn start_file(&mut self, query: &str) {
        self.mode = CompletionMode::File;
        self.query = query.to_string();
        self.init_file_matcher();
        self.update_file_items();
        self.selected = 0;
    }

    /// Update query.
    pub fn update_query(&mut self, query: &str) {
        self.query = query.to_string();
        match self.mode {
            CompletionMode::Slash => self.update_slash_items(),
            CompletionMode::File => self.update_file_items(),
            CompletionMode::None => {}
        }
        // Reset selection if out of bounds
        if self.selected >= self.items.len() && !self.items.is_empty() {
            self.selected = self.items.len() - 1;
        }
    }

    /// Cancel completion.
    pub fn cancel(&mut self) {
        self.mode = CompletionMode::None;
        self.query.clear();
        self.items.clear();
        self.selected = 0;
        self.file_matcher = None;
        self.file_matcher_dirty = false;
    }

    /// Move selection up.
    pub const fn prev(&mut self) {
        if !self.items.is_empty() {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    /// Move selection down.
    pub fn next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1).min(self.items.len() - 1);
        }
    }

    /// Get the selected item for completion.
    #[must_use]
    pub fn complete(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }

    /// Update slash command items based on query.
    fn update_slash_items(&mut self) {
        let query_lower = self.query.to_lowercase();
        self.items = SlashCommand::ALL
            .into_iter()
            .filter(|&command| self.sandbox_available || command != SlashCommand::Sandbox)
            .map(SlashCommand::spec)
            .filter(|spec| {
                query_lower.is_empty() || spec.name.to_lowercase().contains(&query_lower)
            })
            .map(|spec| CompletionItem {
                label: spec.name.to_string(),
                insert: spec.name.to_string(),
                description: spec.description.to_string(),
            })
            .collect();
    }

    /// The labels of the current items.
    #[cfg(test)]
    #[must_use]
    pub fn labels(&self) -> Vec<&str> {
        self.items.iter().map(|item| item.label.as_str()).collect()
    }

    /// Initialize file matcher; the directory walk runs off the UI loop on
    /// a blocking worker and pushes entries into the matcher's injector as
    /// it finds them.
    fn init_file_matcher(&mut self) {
        let config = Config::DEFAULT.match_paths();
        let matcher: Nucleo<PathBuf> = Nucleo::new(config, std::sync::Arc::new(|| {}), None, 1);
        let injector = matcher.injector();
        let workdir = self.workdir.clone();
        let sandbox = self.sandbox.clone();
        blocking::unblock(move || {
            // Add files from workdir (higher priority - add first)
            if workdir.exists() {
                Self::add_files_to_matcher(&injector, &workdir);
            }

            // Add files from sandbox
            if sandbox.exists() && sandbox != workdir {
                Self::add_files_to_matcher(&injector, &sandbox);
            }
        })
        .detach();

        self.file_matcher = Some(matcher);
        // The matcher starts empty; the first refresh fills it as the walk
        // streams entries in.
        self.file_matcher_dirty = true;
    }

    /// Add files from a directory to the matcher.
    fn add_files_to_matcher(injector: &nucleo::Injector<PathBuf>, dir: &Path) {
        let walker = WalkBuilder::new(dir)
            .hidden(true) // Skip hidden files/directories (including .git)
            .ignore(true) // Respect .ignore files
            .git_ignore(true) // Respect .gitignore
            .git_global(true) // Respect global gitignore
            .git_exclude(true) // Respect .git/info/exclude
            .max_depth(Some(10))
            .build();

        for entry in walker.flatten() {
            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                let path = entry.into_path();
                // Create a display path relative to the base
                let display = path
                    .strip_prefix(dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                let _ = injector.push(path, |path, cols| {
                    cols[0] = Utf32String::from(
                        path.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("")
                            .to_string()
                            + " "
                            + &display,
                    );
                });
            }
        }
    }

    /// Re-sync the file matcher without blocking: the worker thread does
    /// the matching and the directory walk feeds it asynchronously, so a
    /// `tick(0)` only picks up results that are ready. The event loop's
    /// ticks keep this current while matches stream in.
    pub fn refresh_file_matches(&mut self) {
        if self.mode != CompletionMode::File {
            return;
        }
        let Some(ref mut matcher) = self.file_matcher else {
            return;
        };
        let status = matcher.tick(0);
        if status.changed || self.file_matcher_dirty {
            self.file_matcher_dirty = status.running;
            self.rebuild_file_items();
        }
    }

    /// Update file items based on query: re-parse the pattern, take a
    /// non-blocking snapshot and rebuild the visible items from it.
    fn update_file_items(&mut self) {
        let Some(ref mut matcher) = self.file_matcher else {
            return;
        };

        // Update pattern
        matcher.pattern.reparse(
            0,
            &self.query,
            nucleo::pattern::CaseMatching::Smart,
            nucleo::pattern::Normalization::Smart,
            false,
        );
        let status = matcher.tick(0);
        self.file_matcher_dirty = status.running;
        self.rebuild_file_items();
    }

    /// Rebuild `self.items` from the matcher's current snapshot.
    fn rebuild_file_items(&mut self) {
        let Some(ref matcher) = self.file_matcher else {
            return;
        };

        // Get snapshot
        let snapshot = matcher.snapshot();

        // Get home directory for ~ substitution
        let home_dir = dirs::home_dir();

        // Collect top matches
        // Note: Agent runs from sandbox dir, so:
        // - Sandbox files: relative path (agent can access directly)
        // - Workdir files: absolute path (agent needs full path)
        // - Home files: ~/... (shell expands ~)
        self.items = snapshot
            .matched_items(0..snapshot.matched_item_count().min(50))
            .map(|item| {
                let path = item.data;
                // Compute display path based on location
                let display_path = if path.starts_with(&self.sandbox) {
                    // Sandbox files: relative path (agent's cwd is sandbox)
                    path.strip_prefix(&self.sandbox).map_or_else(
                        |_| path.to_string_lossy().to_string(),
                        |p| p.to_string_lossy().to_string(),
                    )
                } else if let Some(ref home) = home_dir {
                    if path.starts_with(home) {
                        // Files under home directory: use ~ prefix
                        let rel = path.strip_prefix(home).map_or_else(
                            |_| path.to_string_lossy().to_string(),
                            |p| p.to_string_lossy().to_string(),
                        );
                        format!("~/{rel}")
                    } else {
                        // Absolute path outside home
                        path.to_string_lossy().to_string()
                    }
                } else {
                    // No home dir, use absolute
                    path.to_string_lossy().to_string()
                };
                CompletionItem {
                    label: display_path.clone(),
                    insert: display_path,
                    description: String::new(),
                }
            })
            .collect();
    }
}

impl Default for Completion {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::Completion;

    #[test]
    fn slash_arguments_do_not_match_and_do_not_accept_enter() {
        let mut completion = Completion::new();
        completion.start_slash("resume-claude 4928f530-2440-4156-a83d-fa44be29465a");
        assert!(
            completion.is_active(),
            "slash mode stays on so further edits can recover matches"
        );
        assert!(completion.is_empty());
        assert!(
            !completion.has_matches(),
            "Enter must submit `/resume-claude <id>` instead of swallowing it"
        );
        assert!(completion.complete().is_none());
    }

    #[test]
    fn slash_command_name_still_accepts_enter() {
        let mut completion = Completion::new();
        completion.start_slash("resume-claude");
        assert!(completion.has_matches());
        assert_eq!(
            completion.complete().map(|item| item.insert.as_str()),
            Some("/resume-claude")
        );
    }

    #[test]
    fn two_word_slash_command_still_accepts_enter() {
        let mut completion = Completion::new();
        completion.start_slash("context dump");
        assert!(completion.has_matches());
        assert_eq!(
            completion.complete().map(|item| item.insert.as_str()),
            Some("/context dump")
        );
    }

    #[test]
    fn slash_prefix_still_accepts_enter() {
        let mut completion = Completion::new();
        completion.start_slash("resu");
        assert!(completion.has_matches());
        assert_eq!(
            completion.complete().map(|item| item.insert.as_str()),
            Some("/resume-codex")
        );
    }
}
