//! The table of slash commands the TUI handles.
//!
//! [`SlashCommand`] is the one list: the menu dispatch matches on it
//! exhaustively, the completion palette lists [`SlashCommand::ALL`], and the
//! help screen renders from the same entries.

/// A slash command the TUI handles; each variant is the command of the same
/// name, described by its [`SlashCommand::spec`] row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs, reason = "the spec row documents every variant")]
pub enum SlashCommand {
    Help,
    Session,
    Archive,
    New,
    Profile,
    Model,
    Effort,
    Sandbox,
    Compact,
    Recap,
    Context,
    Usage,
    ContextDump,
    Rewind,
    Jobs,
    Questions,
    Yolo,
    Sleep,
    SleepOff,
    Btw,
    Progress,
    ResumeCodex,
    ResumeClaude,
    ImportClaude,
    ImportCodex,
    Secret,
    Connect,
    Mcp,
    Ssh,
    Cloud,
    Skill,
    Permission,
    Config,
    Login,
    Bug,
    Quit,
}

/// One row of the slash command table.
#[derive(Debug, Clone, Copy)]
pub struct SlashSpec {
    /// The command, with its leading `/`.
    pub name: &'static str,
    /// Hint for the arguments the command takes, empty when it takes none.
    pub args: &'static str,
    /// What the command does.
    pub description: &'static str,
}

impl SlashCommand {
    /// Every command, in the order completion and help list them.
    pub const ALL: [Self; 36] = [
        Self::Help,
        Self::Session,
        Self::Archive,
        Self::New,
        Self::Profile,
        Self::Model,
        Self::Effort,
        Self::Sandbox,
        Self::Compact,
        Self::Recap,
        Self::Context,
        Self::Usage,
        Self::ContextDump,
        Self::Rewind,
        Self::Jobs,
        Self::Questions,
        Self::Yolo,
        Self::Sleep,
        Self::SleepOff,
        Self::Btw,
        Self::Progress,
        Self::ResumeCodex,
        Self::ResumeClaude,
        Self::ImportClaude,
        Self::ImportCodex,
        Self::Secret,
        Self::Connect,
        Self::Mcp,
        Self::Ssh,
        Self::Cloud,
        Self::Skill,
        Self::Permission,
        Self::Config,
        Self::Login,
        Self::Bug,
        Self::Quit,
    ];

    /// The command's table row.
    #[must_use]
    pub const fn spec(self) -> SlashSpec {
        let (name, args, description) = match self {
            Self::Help => ("/help", "", "Show commands and shortcuts"),
            Self::Session => ("/session", "", "Switch or create sessions"),
            Self::Archive => ("/archive", "", "Open an archived session"),
            Self::New => (
                "/new",
                "[owner/repo|path]",
                "Start a new session, optionally on a repository",
            ),
            Self::Profile => ("/profile", "", "Switch profile (Ctrl+P)"),
            Self::Model => ("/model", "[query]", "Switch model for this session"),
            Self::Effort => ("/effort", "", "Adjust reasoning effort for this session"),
            Self::Sandbox => ("/sandbox", "", "Open sandbox folder in Finder"),
            Self::Compact => ("/compact", "[focus]", "Compact conversation history"),
            Self::Recap => ("/recap", "", "Catch-up summary of this session"),
            Self::Context => ("/context", "", "Show context window usage"),
            Self::Usage => ("/usage", "", "Show session API usage and cost"),
            Self::ContextDump => (
                "/context dump",
                "",
                "Dump the verbatim assembled context to a file",
            ),
            Self::Rewind => (
                "/rewind",
                "",
                "Rewind to an earlier prompt (conversation, optionally code too)",
            ),
            Self::Jobs => ("/jobs", "", "Background jobs and running subagents"),
            Self::Questions => ("/questions", "", "Answer the questions the agent left open"),
            Self::Yolo => (
                "/yolo",
                "",
                "Toggle granting every approval request in this session",
            ),
            Self::Sleep => (
                "/sleep",
                "",
                "Tell every session you are asleep: agents keep working and collect questions",
            ),
            Self::SleepOff => ("/sleep off", "", "End the sleep state"),
            Self::Btw => (
                "/btw",
                "",
                "Ask a side question without touching conversation history",
            ),
            Self::Progress => (
                "/progress",
                "",
                "Report the remaining work with a progress bar, without touching the conversation",
            ),
            Self::ResumeCodex => (
                "/resume-codex",
                "[query]",
                "Import a Codex transcript into a new session",
            ),
            Self::ResumeClaude => (
                "/resume-claude",
                "[query]",
                "Import a Claude Code session (opens picker)",
            ),
            Self::ImportClaude => (
                "/import-claude",
                "",
                "Import Claude Code's global rules and skills",
            ),
            Self::ImportCodex => (
                "/import-codex",
                "",
                "Import Codex's global rules and skills",
            ),
            Self::Secret => (
                "/secret",
                "KEY VALUE",
                "Store a secret in the OS keychain (injected into shell env)",
            ),
            Self::Connect => (
                "/connect",
                "[command args]",
                "Use an installed ACP agent as a subagent, or connect a command directly",
            ),
            Self::Mcp => ("/mcp", "", "Manage MCP servers"),
            Self::Ssh => ("/ssh", "", "Manage preconfigured SSH servers"),
            Self::Cloud => (
                "/cloud",
                "",
                "Set up this profile's cloud machines (GitHub Codespaces)",
            ),
            Self::Skill => ("/skill", "", "Manage installed skills"),
            Self::Permission => ("/permission", "", "Re-decide the launch directory's access"),
            Self::Config => ("/config", "", "Open configuration"),
            Self::Login => (
                "/login",
                "",
                "Sign in to a provider account (OpenAI, Grok, OpenRouter)",
            ),
            Self::Bug => (
                "/bug",
                "",
                "Save a bug snapshot (recent logs + session info) to ~/.may/bugs",
            ),
            Self::Quit => ("/quit", "", "Exit May"),
        };
        SlashSpec {
            name,
            args,
            description,
        }
    }

    /// The command, with its leading `/`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.spec().name
    }

    /// The command named exactly `name`, with its leading `/`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|command| command.name() == name)
    }
}

/// The help screen: the command table and the keyboard shortcuts.
#[derive(Debug, askama::Template)]
#[template(path = "tui_help.txt", escape = "none")]
pub struct HelpScreen {
    commands: [SlashSpec; SlashCommand::ALL.len()],
}

impl HelpScreen {
    /// The help screen over every slash command.
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: SlashCommand::ALL.map(SlashCommand::spec),
        }
    }
}

impl Default for HelpScreen {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use askama::Template;

    use super::{HelpScreen, SlashCommand};

    #[test]
    fn names_are_unique_slash_commands() {
        for (index, command) in SlashCommand::ALL.into_iter().enumerate() {
            let name = command.name();
            assert!(name.starts_with('/'), "{name} lacks its slash");
            assert_eq!(
                SlashCommand::from_name(name),
                Some(command),
                "{name} is listed twice"
            );
            assert_eq!(SlashCommand::ALL[index], command);
        }
    }

    #[test]
    fn help_lists_every_command_and_not_clear() {
        let help = HelpScreen::new().render().expect("help renders");
        for command in SlashCommand::ALL {
            let spec = command.spec();
            assert!(help.contains(spec.name), "help omits {}", spec.name);
            assert!(
                help.contains(spec.description),
                "help omits the description of {}",
                spec.name
            );
        }
        assert!(!help.contains("/clear"));
    }

    #[test]
    fn completion_lists_every_command() {
        let mut completion = crate::tui::completion::Completion::new();
        completion.start_slash("");
        let labels = completion.labels();
        assert_eq!(labels.len(), SlashCommand::ALL.len());
        for command in SlashCommand::ALL {
            assert!(labels.contains(&command.name()), "{}", command.name());
        }
    }
}
