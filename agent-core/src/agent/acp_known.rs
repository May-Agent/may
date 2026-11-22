//! Launch commands of ACP agents commonly installed on a developer machine.
//!
//! `/connect` offers the entries whose executable is on `PATH` and which then
//! answer ACP `initialize`. Each command line below was checked against its
//! project's own documentation or `--help`.

/// How to launch a known ACP agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownAcpAgent {
    /// Executable looked up on `PATH`.
    pub command: &'static str,
    /// Arguments that make it speak ACP on stdio.
    pub args: &'static [&'static str],
}

/// The known launch commands.
pub const KNOWN_ACP_AGENTS: &[KnownAcpAgent] = &[
    // `devin acp --help`: "Run as an ACP (Agent Client Protocol) server over stdio".
    KnownAcpAgent {
        command: "devin",
        args: &["acp"],
    },
    // The `codex-acp` binary (npm `@zed-industries/codex-acp`, now
    // `@agentclientprotocol/codex-acp`) is itself the ACP server.
    KnownAcpAgent {
        command: "codex-acp",
        args: &[],
    },
    // npm `@agentclientprotocol/claude-agent-acp`, the renamed successor of
    // `@zed-industries/claude-code-acp`; its `bin` is `claude-agent-acp`.
    KnownAcpAgent {
        command: "claude-agent-acp",
        args: &[],
    },
    // npm `@zed-industries/claude-code-acp` (deprecated by the rename above);
    // its `bin` is `claude-code-acp`.
    KnownAcpAgent {
        command: "claude-code-acp",
        args: &[],
    },
    // `gemini --help`: `--acp` "Starts the agent in ACP mode";
    // `--experimental-acp` is its deprecated spelling.
    KnownAcpAgent {
        command: "gemini",
        args: &["--acp"],
    },
    // goose documentation `documentation/docs/gdk/acp/index.md`: "Launch
    // `goose acp` as a subprocess"; `goose acp` is a CLI subcommand.
    KnownAcpAgent {
        command: "goose",
        args: &["acp"],
    },
    // `opencode acp --help`: "Start an Agent Client Protocol server".
    KnownAcpAgent {
        command: "opencode",
        args: &["acp"],
    },
];
