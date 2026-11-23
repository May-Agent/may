//! The subagent-run snapshot the tracker reports.

use std::time::Instant;

/// Point-in-time view of one tracked subagent run, for rendering.
#[derive(Debug, Clone)]
pub struct SubagentRunSnapshot {
    /// The run's id in the session's [`SubagentRuns`].
    pub run_id: u64,
    /// Subagent type name the call invoked.
    pub agent: String,
    /// Full prompt handed to the subagent.
    pub prompt: String,
    /// When the run started.
    pub started_at: Instant,
    /// Prompt tokens consumed so far.
    pub prompt_tokens: u64,
    /// Completion tokens produced so far.
    pub completion_tokens: u64,
    /// LLM responses the run has observed so far.
    pub requests: u64,
}

impl SubagentRunSnapshot {
    /// Seconds elapsed since the run started.
    #[must_use]
    pub fn elapsed_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    /// Total tokens observed so far.
    #[must_use]
    pub const fn total_tokens(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}
