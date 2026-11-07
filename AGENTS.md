# AGENTS.md

This file provides guidance to coding agents working in this repository.

## Build & Development Commands

```bash
cargo build                    # Build debug
cargo run --bin may            # Run TUI application
cargo test                     # Run all tests
cargo test test_name           # Run specific test
cargo check                    # Type-check without building
cargo fmt                      # Format code
cargo clippy                   # Run lints
```

## The May daemon

One resident daemon runs every agent loop, the task scheduler and search indexing. The TUI, one-shot runs (`may "<prompt>"`) and the web UI are its clients.

```bash
cargo run --bin may -- daemon            # run the daemon in the foreground
cargo run --bin may -- daemon status     # pid, login service, search index backlog
cargo run --bin may -- daemon stop
cargo run --bin may -- daemon install    # launchd agent / systemd user unit
```

- It listens on the unix socket `~/.may/daemon.sock` (mode 0600, used by the TUI and one-shot runs) and on TCP `127.0.0.1:8787` (`[daemon] port`) — the local listener, trusting the machine's own user. A second remote listener on `127.0.0.1:0` serves the same routes behind an identity layer; `[daemon] listen` adds more addresses applied live; a non-loopback one is served directly and identifies each peer through tailscaled's `WhoIs`.
- `~/.may/daemon.lock` keeps it single-instance. Every client starts it on demand and replaces it when its build id differs from the client's, so after a rebuild the next `cargo run` restarts it.
- Remote access is `tailscale serve` in front of the remote listener (`[remote_access]`): every remote request needs a trusted identity — a listed `Tailscale-User-Login` today — or gets a 401. The one public entry is `[remote_access] funnel`: `tailscale funnel --set-path` exposes `/api/tasks/webhook` through a separate loopback listener whose router holds the webhook route alone (`build_webhook_router`); deliveries authenticate by token and signature.

## Fast Web UI + Rust API Iteration Workflow

Use this workflow when iterating on `may rest` + web UI together:

1. Run the daemon in the foreground in API-only mode (default, fastest); `may rest` is the same as `may daemon`:

```bash
cargo run --bin may -- rest --port 8787
```

- The `ui` Cargo feature is **off by default**, so `build.rs` skips `web-ui` production build/embed.
- `may ui` fails fast without that feature; use `may rest` for API-only iteration.
- This keeps Rust incremental compiles fast.
- After completing any Rust code change, restart the Rust API server before validation (`may daemon stop`, or just restart the foreground process).

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
- `build.rs` watches the `web-ui` tree and ignores generated/dependency directories, so frontend source updates trigger a fresh embed build.
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
│  + heel (native sandbox)                                    │
└──────────────────────────────────────────────────────────────┘
```

- **aither-agent**: Core `Agent` abstraction, `TerminalAgentBuilder`, event streaming (`AgentEvent`), subagents (`subagent` command and built-in types), skill loading, compaction
- **aither-cloud**: Provider implementations (Claude, Gemini, OpenAI) via `CloudProvider` enum; `aither-copilot` and `aither-codex` add the Copilot and ChatGPT-subscription backends
- **aither-sandbox**: `TerminalTool`/`TerminalMode` (legacy names for terminal-command execution), `PermissionHandler` trait, the `monitor` tool, background jobs, stdin watch, output overflow
- **aither-models**: Model metadata database for tier lookups
- **aither-mcp**, **aither-browser**, **aither-acp**, **aither-connectivity**: MCP servers as terminal commands, the managed browser, ACP agents as subagent types, connectivity checks
- **heel**: Native sandbox for isolated command execution (managed binary configured in `src/heel/`)

### Module Structure

**`src/agent/`** - MayAgent wrapper
- `may.rs`: Wraps `aither_agent::Agent`: skill and subagent registration, compaction model, host load monitor, permission handling, and session context
- `runtime_context.rs`: renders the session system prompt, one typed template per section under `templates/prompt/` in the order `tag::layout` gives, with memory, `AGENTS.md` and workdirs last as aither external-content blocks; `permission.rs`: `PermissionApprover` for `unsafe` mode; `*_tool.rs`: the IPC commands (`ask_user`, `issue`, `pr`, `repo`, `session`, `task`, `notify`, `tunnel`, `browser`, `computer`, `register-skills`)

**`src/terminal_commands.rs`** - `register_shared_terminal_commands`, which registers every IPC command; `src/terminal_permission_handler.rs` applies the network policy and audit log

**Other top-level modules** - `agent_link` (main agents on one machine exchange messages), `computer` (a virtual desktop per session), `container` (container sandbox), `db` (SQLite persistence: `~/.may/history.db`), `issue` (May and GitHub issue backends), `pr` (session-owned PRs), `repo_clone` (per-session repository clones), `profile` (profiles), `memory` (profile memory extraction), `mcp`, `notification` (web push), `scheduler` and `task_trigger` (scheduled tasks), `skill` (skill management), `heel` (managed sandbox binary), `cache_lifetime` (provider prompt-cache lifetime)

**`src/daemon/`** - The resident daemon and its API
- `mod.rs`: `run_daemon`, single-instance lock, listeners, auto-index
- `serve.rs`: hyper accept loops (unix socket, TCP) calling skyzen's `Router::go`
- `lifecycle.rs`: `ensure_running` (auto-spawn, build-id replacement), service install/uninstall, stop
- `server.rs`: `start` builds `DaemonState`, the REST/SSE router and background workers
- `chat/mod.rs`: the one turn runner (`run_agent_loop`); `chat/hub.rs` `TurnHub` fans a session's turn out as `session_turn` / `session_agent` events, with replay of the turn in progress for late subscribers
- `chat/hosts.rs`: the session's idle agent between turns: the next turn continues with it unless what it was built from changed (`agent_build_key`), so the prompt cache stays warm and detached background commands keep running; it answers `/compact`, `/context`, `/btw`, and a detached command that finishes resumes the session (`start_background_turn`)
- `chat/background.rs`: each session's terminal jobs and subagent runs across agent rebuilds; REST under `/api/sessions/{id}/background|jobs|subagents`, pushed as `session_background`
- `remote_access.rs`: Tailscale serve

**`src/client/`** - `DaemonClient`: REST over the unix socket, and the `/api/events` SSE parser

**`src/repo_search/`** - The repo-search daemon client. Its `search` command searches code only; sessions are searched with `session search` and issues with `issue search`, each over its own collection (`may-sessions`, `may-issues-*`). Also the SQLite `search_outbox` and auto-indexing of frequently used repos

**`src/session/`** - Session lifecycle
- `manager.rs`: Creates/loads/lists sessions, manages current session
- `layout.rs`: typed paths; a session sandbox is `~/.may/profiles/<profile>/sessions/<id>/`
- `store.rs`: creates the session sandbox directories and links its skills and memories
- `runtime.rs`, `runtime_service.rs`, `runs.rs`: per-session runtime state, pending messages (`steer` / `queue`) and live runs
- `history.rs`: in-memory chat history rebuilt from the database (see below)

**`src/tui/`** - Terminal UI (ratatui-based), a daemon client
- `remote.rs`: the `AgentWorker` whose turns run in the daemon; turns other clients start arrive as foreign turns
- `app/mod.rs`: `App` state machine; `AppMode` (in `state.rs`) has `Chat`, `SessionSelect`, `FilePicker`, `Help`, `ToolDetail`, `Config`, the model and effort selectors, the OAuth login modes, permission, workspace and repo-trust dialogs, `AskUser`, `Jobs`, `Btw` and more; `reducer/` handles keys per mode
- `ui/chat.rs`: `ChatStream` containing `ChatElement` variants (UserMessage, AssistantText, ToolCall, TodoBlock, Recap, Error, Notice, Thinking, WorkedFor)
- `event.rs`: Terminal event capture via crossterm
- `completion.rs`: Slash commands (`/help`, `/session`) and file mentions (`@path`)
- `clipboard.rs`: Image/text paste handling

**`src/config.rs`** - Configuration
- `ProviderType` enum with 15 backends (Anthropic, OpenAI, Google, Copilot, Chatgpt, OpenRouter, DeepSeek, Ollama, LMStudio, Groq, Together, Mistral, XAI, Grok, Custom)
- Tier-based model selection: Advanced, Balanced, Fast
- Stored at `~/.may/config.toml`

**`src/workspace.rs`** - Permission model
- Sandbox (the session directory `~/.may/profiles/<profile>/sessions/<id>/`): Full read/write
- CWD: Read-only by default
- Workdirs: User-approved directories with read/write (stored in the profile's `workspace.toml`, `~/.may/profiles/<profile>/workspace.toml`)

**`src/bundle/`** - Embedded resources
- Skills in `skills/` are embedded with `include_dir!`; subagent types (`explore`, `plan`, `research`, `fork`) are aither built-ins, not files
- Written (overwriting) to `~/.may/skills/` at startup, copied into each profile's `skills/`, and linked into the session sandbox as `skills`; container sessions mount it

### Session Storage Format

Session conversation state is stored in the SQLite database at `~/.may/history.db`.
That includes message history, runtime state, context snapshots, and other session metadata.
Session directories only hold sandbox-visible artifacts such as `tmp/`, `files/`, `outputs/`, `artifacts/`, `data/`, `skills/`, and optional `tasks.md`.
Do not treat `.txt` or `.md` files as the canonical session persistence layer.

### Event Loop Architecture

Turns run in the daemon. A client sends with `POST /api/sessions/{id}/messages` (it returns at once) and follows the turn on `/api/events?topics=session_agent,session_turn&sessionId=…`; every attached client sees the same events.

```
TUI main loop:
  1. Draw UI via terminal.draw()
  2. Check message queue for pending messages
  3. Race: terminal events vs agent events forwarded from the daemon's stream
  4. Dispatch to App::handle_event() or App::handle_agent_event()

During streaming:
  - AgentEvent::Text → ChatStream::push_text()
  - AgentEvent::ToolCallStart → ChatStream::push_tool()
  - AgentEvent::Complete → ChatStream::end_text()
```

# Terminal-first tool calling
The native tools are `terminal`, `monitor`, the terminal controls (`terminal_kill`, `terminal_input`, `terminal_read`) and `compact` (aither's `TerminalAgentBuilder` registers it and arms the one-shot pre-compaction alert; its description carries the when-to-compact rule, so May's prompt does not repeat it). This describes command execution capability and does not imply a specific shell implementation.

Everything else is exposed as CLI commands through the terminal tool's IPC bridge rather than as additional native tools.
A tool the model provider executes server-side is the exception: it is passed to the provider natively and, while enabled, replaces the corresponding terminal command. Web search is always that hosted tool (`web_search` / `google_search`); May does not register a `websearch` command or a client-side search engine. High-frequency browser work should use the May-managed `browser` command rather than ad-hoc shell recipes or direct browser installers.

If the agent ends up using the same non-native workflow frequently, promote that workflow into a proper `Tool` implementation and let May auto-convert it into a terminal command through the existing Tool-trait CLI bridge.
Do not leave high-frequency capabilities as ad-hoc shell snippets the agent has to reconstruct manually every time.

### Key Patterns

- **State machine UI**: `AppMode` enum controls which handler processes keys
- **Streaming priority**: Agent events drained before terminal events to ensure responsive streaming display
- **Scroll sentinel**: `scroll = usize::MAX` means "follow bottom", normalized lazily
- **Provider abstraction**: All providers unified through `CloudProvider` enum from aither-cloud
- **Permission handler**: `TerminalPermissionHandler` implements `aither_sandbox::PermissionHandler` and delegates `unsafe` approvals to `PermissionApprover`


# Bad smell
- .chars is expensive
- [..len] is forbidden since it can panic for non-ascii

## Rules

- Use modern techniques and best practices, for instance, use bun instead of npm
- Never concat strings by hand, utilize type-safed template
- Agent context is expensive. Keep injected agent-facing context minimal and strictly task-operational.
- Do not inject implementer-facing constraints, Codex instructions, AGENTS.md rules, or user instructions that are only meant for the coding agent into the runtime agent context.
- When tuning prompts or runtime context, only include information the runtime agent must know to act correctly in that session. Keep framework-authoring notes and meta guidance in code/comments/docs instead of the agent prompt.
- Do not inject generic prompting slogans or obvious meta advice into agent context. If a sentence does not change concrete runtime behavior, remove it.
- Runtime prompt/context rendering must use static typed template inputs. Do not branch on enabled/disabled states to hand-build prompt strings in Rust and then pass those strings into templates.
- Text the model reads — tool results, notices, alerts and reminders — is natural language rendered from a typed template; no serializer output (JSON, XML) reaches the model, except data a command or an external tool itself produced and the schema of output the model must emit.
- Prefer runtime-agnostic async primitives. Do not introduce new `tokio::*` filesystem, timer, task, process, or networking usage when an existing runtime-neutral abstraction already fits this codebase (for example `async_fs`, `futures-lite`, `blocking`, `zenwave`, or existing executor abstractions). If a path must work outside a Tokio reactor, keep it Tokio-free.
- **Native tools.** The agent's native tools are `terminal`, `monitor`, the terminal controls (`terminal_kill`, `terminal_input`, `terminal_read`) and `compact`. Every other capability is an IPC CLI command registered through `register_shared_terminal_commands` and invoked from `terminal`, never passed to `TerminalAgentBuilder::tool(...)`. The one exception is a tool the model provider executes server-side, which is passed to the provider natively and, while enabled, replaces the corresponding terminal command. Web search is always hosted; there is no `websearch` IPC command.
- **Sandbox and `unsafe`.** The sandbox exists to help the user and the agent work well and safely and to guard against misalignment, never to stop a task. May keeps `unsafe` as narrow as it can by adding IPC commands and widening what the sandbox covers, but `unsafe` cannot be eliminated: a pattern becomes an IPC command only once it is observed to need `unsafe` frequently, and until then a step the user authorized that the sandbox blocks runs in `unsafe` mode.
- May has no setting that needs a restart to apply; configuration updates live.
- Anything that can block meaningfully — clipboard reads, decode/encode, IO, large terminal payloads — runs off the event loop and off the draw path (`unblock`/`spawn`); the render path only paints prepared state.
- System alerts are event-driven and one-shot: one event injects one alert, when it happens. Compaction drops an alert instead of replaying it, and nothing restates or regenerates it. A condition that occurs again — a rule the agent violates again included — is a new event, alerted again.
- Never rewrite the cached prompt prefix while the provider's cache is still warm; work that rewrites it (reassembly) runs only after the cache lifetime has passed.
- Never drop thinking blocks when a path exists that keeps them valid; dropping them noticeably degrades the model.
- An unanswered `ask_user` question is no authorization: the agent never decides it itself, and the harness never pushes the agent onward while every open task is blocked.
- Subagents stay inside the main agent's authority: they may read issues but never change them, never modify `tasks.md`, never ask the user, and never push or open a pull request; anything that reaches outside goes back to the main agent. A subagent may commit only when authorized, and the main agent reviews every such commit. The harness enforces this; the prompt does not carry it.
- aither provides the simplest, most general agent loop and exposes every capability for may to customise. It carries no terminal-first architecture and no product policy: the terminal tool and its shell, IPC commands, prompt and alert wording, and other product behaviour live in may.
- **Home environment.** A session's home environment is where its agent works: where its tool-call results are stored and its IPC commands execute, and where its subagent, skills and memories directories and `AGENTS.md` live. The session's transcript is not part of it; the daemon keeps it in its one database. Memory and disk pressure handling exists to keep the harness itself running, so it protects the home environment and nothing else. A local session's home environment is the whole memory and disk of the machine the daemon runs on; a container session's adds the container's own limits. A remote environment is never a home environment: a session using one is based on a home session, and the remote side's memory and disk impose nothing. An I/O failure from exhausting the home environment, a crashed container runtime included, is handled and recovered from so a long-running agent survives it.
- Every wait the user sees shows a loading indicator, downloading or preparing tools and environments included; nothing leaves the user on a blank or frozen screen.
- **Web UI transport split — one SSE, REST for everything else.** The web UI must shape its server boundary as: (1) a single unidirectional SSE stream (`/api/events`) carries all server-pushed state and updates, multiplexed by event `kind`; (2) every other one-shot interaction is a REST call. Add a second SSE channel only when it makes the architecture noticeably cleaner (e.g. an isolated, transient per-task event firehose with a different lifecycle from the main stream); otherwise add a new event `kind` to the existing stream. Do not introduce WebSocket, long-poll, or duplicate SSE endpoints for ordinary state replication. Server-pushed cache invalidations (model lists, config changes, etc.) ride the existing stream as new event kinds.
