//! The foreign agents a May session can be seeded from.

use std::path::{Path, PathBuf};

use jiff::Timestamp;

/// The foreign agent whose transcript is being imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForeignAgent {
    /// `~/.codex/sessions/**/rollout-*.jsonl`.
    Codex,
    /// `~/.claude/projects/<cwd-slug>/<session>.jsonl`.
    Claude,
}

/// One foreign transcript as it appears in the resume picker.
#[derive(Debug, Clone)]
pub struct ForeignSessionSummary {
    /// Foreign session id (Claude: jsonl file stem).
    pub id: String,
    /// Transcript path; passed back to [`import`] as the query.
    pub path: PathBuf,
    /// Working directory recorded in the transcript, when found.
    pub cwd: Option<String>,
    /// First real user prompt, truncated for the picker.
    pub title: String,
    /// File mtime, used to sort newest first.
    pub modified: Option<Timestamp>,
}

impl ForeignAgent {
    /// Slash command name for this source.
    #[must_use]
    pub const fn command(&self) -> &'static str {
        match self {
            Self::Codex => "/resume-codex",
            Self::Claude => "/resume-claude",
        }
    }

    /// Short source label used in titles and notices.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    /// The source named by [`Self::label`].
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        [Self::Codex, Self::Claude]
            .into_iter()
            .find(|agent| agent.label() == label)
    }

    /// Slash command that imports this agent's global rules and skills.
    #[must_use]
    pub const fn rules_command(&self) -> &'static str {
        match self {
            Self::Codex => "/import-codex",
            Self::Claude => "/import-claude",
        }
    }

    /// The agent's product name, as the user knows it.
    #[must_use]
    pub const fn product_name(&self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }

    /// The agent's global rules file under the user's home `home`.
    #[must_use]
    pub fn rules_file(&self, home: &Path) -> PathBuf {
        match self {
            Self::Codex => home.join(".codex/AGENTS.md"),
            Self::Claude => home.join(".claude/CLAUDE.md"),
        }
    }

    /// The agent's skills directory under the user's home `home`.
    #[must_use]
    pub fn skills_dir(&self, home: &Path) -> PathBuf {
        match self {
            Self::Codex => home.join(".codex/skills"),
            Self::Claude => home.join(".claude/skills"),
        }
    }
}
