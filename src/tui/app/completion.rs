//! Composer completion logic for slash commands and `@` file mentions.

use super::super::completion::CompletionMode;
use super::App;

impl App {
    /// Update completion state based on current input.
    pub(super) fn update_completion_state(&mut self) {
        // Check for slash command at start
        if self.input.starts_with('/') {
            let query = &self.input[1..]; // Skip the /
            if self.completion.mode == CompletionMode::Slash {
                self.completion.update_query(query);
            } else {
                self.completion.start_slash(query);
            }
            return;
        }

        // Check for @ file mention - find the last @ that starts a mention
        if let Some(at_byte_pos) = self.find_active_mention() {
            let query = &self.input[at_byte_pos + 1..]; // +1 is safe since @ is ASCII
            if self.completion.mode == CompletionMode::File {
                self.completion.update_query(query);
            } else {
                // Set up directories for file search
                let workdir = std::env::current_dir().unwrap_or_default();
                let sandbox = self
                    .session_manager
                    .current()
                    .map_or_else(|| workdir.clone(), |s| s.sandbox_dir.clone());
                self.completion.set_directories(workdir, sandbox);
                self.completion.start_file(query);
            }
            return;
        }

        // No completion trigger found
        if self.completion.is_active() {
            self.completion.cancel();
        }
    }

    /// Byte offset of the active @ mention (one that's being typed).
    fn find_active_mention(&self) -> Option<usize> {
        // Look for @ that's not followed by whitespace and is at cursor or before it
        let mut last_at = None;
        let mut prev_is_space = true;

        for (i, c) in self.input.char_indices() {
            if c == '@' {
                // Check if this @ is the start of a mention (not in middle of word)
                if prev_is_space {
                    last_at = Some(i);
                }
            } else if c.is_whitespace() && last_at.is_some() {
                // Space ends the current mention
                last_at = None;
            }
            prev_is_space = c.is_whitespace();
        }

        // Only return if cursor is within the mention
        if let Some(pos) = last_at
            && self.cursor > pos
        {
            return Some(pos);
        }
        None
    }

    /// Accept a completion and insert it into the input.
    pub(super) fn accept_completion(&mut self, insert: &str) {
        match self.completion.mode {
            CompletionMode::Slash => {
                // Replace the entire input with the command
                self.input = insert.to_string();
                self.cursor = self.input.len();
            }
            CompletionMode::File => {
                // Replace from @ to cursor with the file path
                if let Some(at_pos) = self.find_active_mention() {
                    // Build new input: before @ + @path + space
                    let before = &self.input[..at_pos];
                    let after = &self.input[self.cursor..];
                    self.input = format!("{before}@{insert} {after}");
                    // Position cursor after the inserted path + space
                    self.cursor = at_pos + 1 + insert.len() + 1;
                }
            }
            CompletionMode::None => {}
        }
        self.completion.cancel();
    }
}
