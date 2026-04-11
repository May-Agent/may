//! Composer completion logic for slash commands and `@` file mentions.

use super::super::completion::CompletionMode;
use super::{App, char_to_byte_index};

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
        if let Some(at_char_pos) = self.find_active_mention() {
            // Convert char index to byte index for safe slicing
            let at_byte_pos = char_to_byte_index(&self.input, at_char_pos);
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

    /// Find the position of an active @ mention (one that's being typed).
    fn find_active_mention(&self) -> Option<usize> {
        // Look for @ that's not followed by whitespace and is at cursor or before it
        let input_bytes: Vec<char> = self.input.chars().collect();
        let mut last_at = None;

        for (i, &c) in input_bytes.iter().enumerate() {
            if c == '@' {
                // Check if this @ is the start of a mention (not in middle of word)
                let prev_is_space = i == 0 || input_bytes[i - 1].is_whitespace();
                if prev_is_space {
                    last_at = Some(i);
                }
            } else if c.is_whitespace() && last_at.is_some() {
                // Space ends the current mention
                last_at = None;
            }
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
                self.input_char_count = self.input.chars().count();
                self.cursor = self.input_char_count;
            }
            CompletionMode::File => {
                // Replace from @ to cursor with the file path
                if let Some(at_pos) = self.find_active_mention() {
                    // Build new input: before @ + @path + space
                    let before: String = self.input.chars().take(at_pos).collect();
                    let after: String = self.input.chars().skip(self.cursor).collect();
                    self.input = format!("{before}@{insert} {after}");
                    self.input_char_count = self.input.chars().count();
                    // Position cursor after the inserted path + space
                    self.cursor = at_pos + 1 + insert.chars().count() + 1;
                }
            }
            CompletionMode::None => {}
        }
        self.completion.cancel();
    }
}
