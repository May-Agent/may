//! Input completion for slash commands and file mentions.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use nucleo::{Config, Nucleo, Utf32String};

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

/// Slash command definitions.
pub static SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/session", "Switch or create sessions"),
    ("/new", "Start a new session"),
    ("/profile", "Switch profile (Ctrl+P)"),
    ("/model", "Switch model for this session"),
    ("/sandbox", "Open sandbox folder in Finder"),
    ("/compact", "Compact conversation history"),
    ("/mcp", "Manage MCP servers"),
    ("/ssh", "Manage preconfigured SSH servers"),
    ("/skill", "Manage installed skills"),
    ("/config", "Open configuration"),
    ("/quit", "Exit May"),
];

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
    /// Working directory (higher priority).
    workdir: PathBuf,
    /// Sandbox directory (lower priority).
    sandbox: PathBuf,
}

impl Completion {
    /// Create a new completion state.
    pub fn new() -> Self {
        Self {
            mode: CompletionMode::None,
            query: String::new(),
            items: Vec::new(),
            selected: 0,
            file_matcher: None,
            workdir: PathBuf::new(),
            sandbox: PathBuf::new(),
        }
    }

    /// Set directories for file completion.
    pub fn set_directories(&mut self, workdir: PathBuf, sandbox: PathBuf) {
        self.workdir = workdir;
        self.sandbox = sandbox;
    }

    /// Check if completion is active.
    pub fn is_active(&self) -> bool {
        self.mode != CompletionMode::None
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
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Get items count.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
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
    }

    /// Move selection up.
    pub fn prev(&mut self) {
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
    pub fn complete(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }

    /// Update slash command items based on query.
    fn update_slash_items(&mut self) {
        let query_lower = self.query.to_lowercase();
        self.items = SLASH_COMMANDS
            .iter()
            .filter(|(cmd, _)| {
                if query_lower.is_empty() {
                    true
                } else {
                    cmd.to_lowercase().contains(&query_lower)
                }
            })
            .map(|(cmd, desc)| CompletionItem {
                label: cmd.to_string(),
                insert: cmd.to_string(),
                description: desc.to_string(),
            })
            .collect();
    }

    /// Initialize file matcher with files from directories.
    fn init_file_matcher(&mut self) {
        let config = Config::DEFAULT.match_paths();
        let matcher: Nucleo<PathBuf> = Nucleo::new(config, std::sync::Arc::new(|| {}), None, 1);

        // Add files from workdir (higher priority - add first)
        if self.workdir.exists() {
            self.add_files_to_matcher(&matcher, &self.workdir, true);
        }

        // Add files from sandbox
        if self.sandbox.exists() && self.sandbox != self.workdir {
            self.add_files_to_matcher(&matcher, &self.sandbox, false);
        }

        self.file_matcher = Some(matcher);
    }

    /// Add files from a directory to the matcher.
    fn add_files_to_matcher(&self, matcher: &Nucleo<PathBuf>, dir: &Path, _is_workdir: bool) {
        let injector = matcher.injector();

        let walker = WalkBuilder::new(dir)
            .hidden(true) // Skip hidden files/directories (including .git)
            .ignore(true) // Respect .ignore files
            .git_ignore(true) // Respect .gitignore
            .git_global(true) // Respect global gitignore
            .git_exclude(true) // Respect .git/info/exclude
            .max_depth(Some(10))
            .build();

        for entry in walker.flatten() {
            if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
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

    /// Update file items based on query.
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

        // Tick to process
        matcher.tick(100);

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
                    path.strip_prefix(&self.sandbox)
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| path.to_string_lossy().to_string())
                } else if let Some(ref home) = home_dir {
                    if path.starts_with(home) {
                        // Files under home directory: use ~ prefix
                        let rel = path
                            .strip_prefix(home)
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|_| path.to_string_lossy().to_string());
                        format!("~/{}", rel)
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
