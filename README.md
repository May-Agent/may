# May

**A daemon-backed, terminal-first AI coding agent harness built in Rust.**

May is an autonomous software engineering assistant built on the [Aither](https://github.com/May-Agent/aither) framework, [Heel](https://github.com/May-Agent/heel) native OS sandbox, and [repo-search](https://github.com/May-Agent/repo-search) Apple Neural Engine (ANE) code indexer. It pairs a resident background daemon (`may daemon`) with a fast terminal UI (`ratatui`), one-shot CLI execution (`may "<prompt>"`), and a real-time Web UI.

## Architectural Highlights

- **Terminal-First IPC Tool Bridge** — Instead of bloating the model's context with dozens of JSON tool schemas, May exposes only `terminal`, `monitor`, terminal process controls (`terminal_kill`, `terminal_input`, `terminal_read`), and `compact` as native tools. Over 15 capabilities (`search`, `browser`, `issue`, `subagent`, `task`, `imagine`, `computer`, `ask`, and active MCP servers) are automatically bridged from Rust `Tool` traits into sandboxed IPC CLI commands inside the agent's shell.
- **Native OS Sandboxing via [Heel](https://github.com/May-Agent/heel)** — Commands execute inside zero-VM native OS sandboxes (`sandbox-exec` SBPL on macOS, Landlock v4 + Seccomp on Linux, AppContainer on Windows) or managed containers, enforcing strict `W^X` (writable paths are non-executable) and proxy-filtered network policies while keeping local GPU/NPU hardware accessible.
- **Per-Session 60fps Virtual Desktops (`src/computer/`)** — On macOS, each session can provision its own isolated 2x Retina 60fps virtual display via `SkyLight` (`CGVirtualDisplay`). May can inspect and drive GUI applications headlessly without hijacking the user's physical screen or cursor, streaming the virtual display live to the Web UI.
- **Apple ANE Semantic Code & Issue Search ([`repo-search`](https://github.com/May-Agent/repo-search))** — Built-in repository, session history, and issue indexing powered by Tree-sitter AST chunking and low-power Apple Neural Engine (`CoreML`) / ONNX vector + FTS5 hybrid retrieval.
- **Strict Context Hygiene & Warm Prompt Caching** —
  - All tool outputs, alerts, and system notices are rendered into clean natural language via static typed `Askama` templates—raw serializer dumps (`JSON`/`XML`) never pollute the model context.
  - `ParkedAgents` keeps idle agents warm between turns and defers system prompt reassembly until the provider's prompt-cache TTL expires.
  - Reasoning / thinking blocks are preserved across turns to prevent model degradation.
- **Harness-Enforced Subagent Authority** — Role-separated tool registration (`main_only` and `configure_role_tools`) physically prevents subagents from calling `ask_user`, mutating issues, modifying `tasks.md`, or pushing unreviewed commits at the Rust type/runtime layer rather than relying on prompt instructions.
- **Resident Single-Instance Daemon & TurnHub** — One daemon (`~/.may/daemon.sock` + loopback HTTP/SSE) drives every agent loop, background job, subagent run, and scheduled task. TUI, CLI, and Web UI clients attach to the same live `TurnHub` stream with automatic `BuildId` hot-replacement on rebuild and optional `tailscale serve` remote access.

## Quick Start

```bash
# Launch the interactive Terminal UI (auto-spawns the background daemon)
cargo run --bin may

# Run a one-shot task from the command line
cargo run --bin may -- "refactor the retry loop and run cargo test"

# Manage the resident daemon
cargo run --bin may -- daemon status
cargo run --bin may -- daemon install   # install launchd agent / systemd user unit

# Run with the embedded production Web UI
cargo run --features ui --bin may -- ui --port 8787
```

## Architecture

```text
┌──────────────────────────────────────────────────────────────────────┐
│                              may-agent                               │
│  ┌──────────────┐  ┌──────────────┐  ┌────────────┐  ┌────────────┐  │
│  │  TUI Client  │  │  Web UI /    │  │  TurnHub & │  │ SkyLight   │  │
│  │  (ratatui)   │  │  SSE Stream  │  │  Scheduler │  │ Virtual UI │  │
│  └──────┬───────┘  └──────┬───────┘  └─────┬──────┘  └─────┬──────┘  │
│         └─────────────────┴────────┬───────┴───────────────┘         │
│                         Resident Daemon (`~/.may/daemon.sock`)       │
└────────────────────────────────────┼─────────────────────────────────┘
                                     ▼
┌──────────────────────────────────────────────────────────────────────┐
│              Aither Ecosystem + Heel + Repo-Search                   │
│  • aither-agent / aither-core / aither-cloud / aither-mcp            │
│  • heel (macOS SBPL, Linux Landlock/Seccomp, Windows AppContainer)   │
│  • repo-search (Apple ANE CoreML + ONNX AST-aware hybrid RAG)        │
└──────────────────────────────────────────────────────────────────────┘
```

## License

MIT License
