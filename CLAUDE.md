# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Development Commands

```bash
cargo build                    # Build debug
cargo build --release          # Build release
cargo run --bin may            # Run TUI application
cargo test                     # Run all tests
cargo test test_name           # Run specific test
cargo check                    # Type-check without building
cargo fmt                      # Format code
cargo clippy                   # Run lints
```

## Fast Web UI + Rust API Iteration Workflow

Use this workflow when iterating on `may ui` + web UI together:

1. Run Rust server in API-only mode (default, fastest):

```bash
cargo run --bin may -- ui --port 8787
```

- The `ui` Cargo feature is **off by default**, so `build.rs` skips `web-ui` production build/embed.
- This keeps Rust incremental compiles fast.
- After completing any Rust code change, restart the Rust API server before validation.

2. Run Vite dev server separately with API proxy:

```bash
cd web-ui
MAY_API_TARGET=http://127.0.0.1:8787 bun run dev
```

- `vite.config.ts` proxies `/api/*` to `MAY_API_TARGET` (defaults to `http://127.0.0.1:8787`).
- You get Vite HMR for frontend + incremental compile for Rust backend.
- In debug/iteration workflows, do **not** run `vite build`.

3. Only build embedded production UI when needed:

```bash
cargo build --features ui
# or
cargo run --features ui --bin may -- ui --port 8787
```

- Enabling feature `ui` triggers `build.rs` to run `bun run build` and embed `web-ui/dist`.
- Use this for release-like verification, not daily inner-loop debugging.

## Architecture Overview

May is a terminal-based AI assistant built as an application layer on the **aither** framework ecosystem.

### Framework Integration

```
┌─────────────────────────────────────────────────────────────┐
│                         may-agent                            │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────────────┐ │
│  │   TUI   │  │ Session │  │  Agent  │  │    Workspace    │ │
│  │ ratatui │  │ Manager │  │ Wrapper │  │   Permissions   │ │
│  └────┬────┘  └────┬────┘  └────┬────┘  └────────┬────────┘ │
└───────┼────────────┼────────────┼────────────────┼──────────┘
        │            │            │                │
┌───────┴────────────┴────────────┴────────────────┴──────────┐
│                      aither framework                        │
│  aither-agent   aither-cloud   aither-sandbox   aither-core │
│                                                              │
│  + leash (native sandbox)                                    │
└──────────────────────────────────────────────────────────────┘
```

- **aither-agent**: Core `Agent` abstraction, `BashAgentBuilder`, event streaming (`AgentEvent`)
- **aither-cloud**: Provider implementations (Claude, Gemini, OpenAI) via `CloudProvider` enum
- **aither-sandbox**: `BashTool`, `BashMode`, `PermissionHandler` trait
- **aither-models**: Model metadata database for tier lookups
- **leash**: Native sandbox for isolated command execution

### Module Structure

**`src/agent/`** - MayAgent wrapper
- `may.rs`: Wraps `aither_agent::Agent` with May-specific system prompt (via Askama template at `templates/system.md`), permission handling, and session context

**`src/session/`** - Session lifecycle
- `manager.rs`: Creates/loads/lists sessions, manages current session
- `store.rs`: Filesystem persistence to `~/.may/sessions/<uuid>/`
- `history.rs`: Chat history with YAML frontmatter format (see format below)

**`src/tui/`** - Terminal UI (ratatui-based)
- `app.rs`: State machine with modes: `Chat`, `SessionSelect`, `FilePicker`, `Help`, `ToolDetail`, `Welcome`, `ProviderConfig`
- `ui/chat.rs`: `ChatStream` containing `ChatElement` variants (UserMessage, AssistantText, ToolCall, TodoBlock, Error, Loading, Thinking)
- `event.rs`: Terminal event capture via crossterm
- `completion.rs`: Slash commands (`/help`, `/session`) and file mentions (`@path`)
- `clipboard.rs`: Image/text paste handling

**`src/config.rs`** - Configuration
- Provider enum with 11 backends (Anthropic, OpenAI, Google, OpenRouter, DeepSeek, Ollama, Groq, Together, Mistral, XAI, Custom)
- Tier-based model selection: Advanced, Balanced, Fast
- Stored at `~/.may/config.toml`

**`src/workspace.rs`** - Permission model
- Sandbox (`~/.may/sessions/<id>/workdir`): Full read/write
- CWD: Read-only by default
- Workdirs: User-approved directories with read/write (stored in `~/.may/workspace.toml`)

**`src/bundle/`** - Embedded resources
- Subagents (`explore.md`, `research.md`, `plan.md`) bundled via `include_str!`
- Extracted to `~/.may/.bundle/` with symlinks at `~/.may/subagents/` and `~/.may/skills/`

### Session Storage Format

Sessions stored at `~/.may/sessions/<uuid>/`:
```
<uuid>/
├── chat.txt       # YAML frontmatter message history
├── workdir/       # Agent sandbox directory
├── bin/           # Session-specific scripts (request_workspace)
└── images/        # Saved clipboard images
```

Chat history format (`chat.txt`):
```markdown
---
role: user
timestamp: 2024-01-15T10:30:00Z
---
Hello!

---
role: assistant
timestamp: 2024-01-15T10:30:05Z
---
Hi! How can I help?
```

### Event Loop Architecture

```
main loop:
  1. Draw UI via terminal.draw()
  2. Check message queue for pending messages
  3. Race: terminal events vs agent stream events
  4. Dispatch to App::handle_event() or App::handle_agent_event()

During streaming:
  - AgentEvent::Text → ChatStream::push_text()
  - AgentEvent::ToolCallStart → ChatStream::push_tool()
  - AgentEvent::Complete → ChatStream::end_text()
```

# Bash-based tool calling
From the perspective of LLM, there is only one tool: `bash`. All capabilities are exposed via CLI commands.
For instance, web search is done by calling `websearch "query"` command in bash.

### Key Patterns

- **State machine UI**: `AppMode` enum controls which handler processes keys
- **Streaming priority**: Agent events drained before terminal events to ensure responsive streaming display
- **Scroll sentinel**: `scroll = usize::MAX` means "follow bottom", normalized lazily
- **Provider abstraction**: All providers unified through `CloudProvider` enum from aither-cloud
- **Permission handler**: `MayPermissionHandler` implements `aither_sandbox::PermissionHandler` trait


# Bad smell
- .chars is expensive
- [..len] is forbidden since it can panic for non-ascii
