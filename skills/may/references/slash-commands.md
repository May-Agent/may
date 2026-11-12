# Slash commands

These are TUI commands. The web UI has no slash palette: its composer treats only the exact texts `/sleep` and `/sleep off` as commands; other things (model, effort, yolo) are controls on the page (see `clients.md`).

## How they are entered

Typing `/` in the TUI input opens a palette filtered as you type. Enter on a palette item runs the command at once; the line never reaches the agent. A slash line sent without choosing from the palette is an ordinary message, except these forms that take arguments and are parsed from the typed line: `/new <owner/repo or path>`, `/compact [focus]`, `/context`, `/context dump`, `/model [query]`, `/connect [command args]`, `/resume-codex [query]`, `/resume-claude [query]`, `/import-claude`, `/import-codex`, `/secret KEY VALUE`, `/bug`. Choosing `/resume-codex` or `/secret` from the palette only puts the command in the input so arguments can be typed. The palette and the `/help` screen list the same commands, from one table.

## Sessions and profiles

- `/new` starts a new session (also Ctrl+N). `/new <owner/repo or path>` starts it directly on that repository, like `may --repo`.
- `/session` opens the recent-session list (search by typing; Enter opens; Tab / Shift+Tab switch profile when more than one exists). `/archive` opens the archived list; opening an archived session unarchives it.
- `/profile` opens the profile picker (also Ctrl+P); with a single profile it opens the session list instead. See `profiles.md`.
- `/rewind` opens a picker of the session's earlier prompts (also Esc twice when idle). Choosing one truncates the conversation to before that prompt and puts its text and attachments back in the input for editing; "(current)" cancels. Refused with a notice while a turn is running, and when there are no prompts.
- `/resume-claude` opens a picker of Claude Code sessions; `/resume-claude <query>` and `/resume-codex <query>` import a transcript into a new session (`src/session/import/`).
- `/import-claude` and `/import-codex` import that tool's global rules and skills: skills are copied, and a session opens that rewrites the rules into `~/.may/AGENTS.md`; when the tool has no rules file only the skills are copied.
- `/sandbox` opens the session sandbox directory in the system file manager (`open`, `xdg-open` or `explorer`).
- `/quit` exits; when the session's agent is working — a run in flight, or background commands or subagent runs alive — a dialog first asks whether to keep it running in the background or stop it. Ctrl+C clears the input and a second Ctrl+C quits; Ctrl+Q quits.

## Model and conversation

- `/model` opens the model picker for this session; `/model <query>` opens it searching for the query. The last chosen model becomes `last_model`, the default for new sessions.
- `/effort` opens a left/right ladder of reasoning efforts for the current model (Enter or Esc closes; each move applies). The last choice is stored as `last_effort`.
- `/compact [focus]` compacts the conversation into a handoff document now; the optional focus steers what it keeps. Compaction also runs by itself (see `configuration.md`, `[context]`).
- `/recap` generates a catch-up summary card for the session. Refused while a turn runs ("run /recap when it finishes"); an empty session answers "nothing to recap". An idle session also gets one automatically (`[recap]`).
- `/context` shows context-window usage; `/context dump` writes the verbatim assembled context to a file in the session (`outputs/context-dump.md`).
- `/usage` shows the session's API usage: time spent in requests against the session's age, cost, a per-model breakdown, and prompt-cache misses by cause. A model's request count notes how many of its requests were keep-warm requests: while a background command outlives the provider's cache lifetime, the idle session sends one single-token request shortly before the cache would cool, repeats it for as long as the command runs, and counts its cost with the session's usage.
- `/btw` opens an overlay for a side question: one lightweight answer with no tools, never written into the conversation, and the main agent keeps running. While the agent has a live context (idle or mid-turn) the answer comes from a read-only fork of that context; it is refused when the prompt cache has gone cold or the context is about to compact. `/progress` works the same way. Esc or q closes it. It needs a started session.

## Work in flight

- `/jobs` lists background commands and running subagents. j/k or arrows move, Enter opens a job's live output (g/G top/bottom, r refresh), x kills a running job or cancels a subagent, r refreshes, Esc or q closes.
- `/questions` opens the questions the agent left open (`ask_user`); repeated use cycles through them. Ctrl+Left does the same. An unanswered question is never treated as permission.
- `/yolo` toggles, for this session only, granting every approval request without asking; it applies from the next request, and grants are recorded in the history like user approvals. Say it again to turn it off. The web UI has the same switch on the session page.
- `/permission` re-asks the launch directory's access decision: the repository trust question inside a git worktree (opened on the level in force), the write-grant question outside one.
- `/sleep` and `/sleep off`: see `sleep.md`.

## Connections and secrets

- `/config` opens the configuration screen (see `configuration.md`).
- `/login` opens a provider sign-in picker with OpenAI (ChatGPT Plus/Pro subscription), xAI Grok (SuperGrok / X Premium+) and OpenRouter; Enter starts that flow. GitHub Copilot sign-in is started from `/config`.
- `/mcp` manages MCP servers: a (add), d (remove), r (reconnect); stored in `~/.may/mcp.json`. Connected servers appear to the agent as terminal commands.
- `/ssh` manages preconfigured SSH servers (`user@host[:port]` targets, a to add, d to remove) and probes a server's OS when it is added, saved in `[[ssh_servers]]`; profiles attach them (see `profiles.md`).
- `/cloud` sets up the current profile's cloud machines, GitHub Codespaces first: s sets it up as a GitHub account of the `gh` login (typed, or empty for gh's active account), d removes it. Setup checks that the account's token has the `codespace` scope; without it the error names the `gh auth refresh --hostname github.com --scopes codespace` command to run. A codespace is created from the session's GitHub repository, so it starts with the code and the project's devcontainer; a session without one uses the private repository `may-environment` May creates in that account, and a public repository of that name is refused. The machine types offered are the ones GitHub lists for that repository. n starts a new session that runs entirely on a codespace: pick a machine type from the list (↑/↓, Enter), and the TUI and web UI stay on this machine while the session's home environment is the codespace. May creates it, installs this build of `may` there (the CI build of its commit, or its release), clones the project when the codespace comes from `may-environment` (a project whose devcontainer has no SSH server, which `gh codespace ssh` needs), and starts the codespace's daemon; each step shows with a spinner until the session is ready. The local daemon reaches it through `gh codespace ports forward` with a token only it holds. The session model's provider goes to the codespace when it comes up and before each message: an API key as configured, while a signed-in provider (Copilot, ChatGPT, Grok) gets only a short-lived access token, which this machine refreshes and pushes again, also during a turn before the pushed one expires; its refresh token never leaves this machine. This machine's fast and balanced models go with it, with the providers they name, and go again when they change here, so the codespace's titles and memory run on them. The codespace keeps what is pushed in memory only, never in its configuration or any file. The session's title and running state show in the local session list as the codespace reports them. A suspended codespace is started again by the session's next message; until then both clients show the session as suspended, waiting for a message to wake it, and its history appears once the codespace runs. Deleting the session deletes its codespace.
- `/skill` lists installed skills; a adds (install from a GitHub repository), d removes; typing filters.
- `/connect` opens a picker of installed ACP agents to use as a subagent type; `/connect <command> [args]` connects that command directly. The connection is stored under `[acp_agents.<name>]` and needs the agent's handshake to finish within `[acp] trial_limit`.
- `/secret KEY VALUE` stores VALUE in the OS keychain, injected into the shell environment as KEY. The line is removed from input history and never enters the transcript or reaches the agent. Stored values are replaced by their names in tool results and provider requests. A missing `~/.may/secrets.json` means no secrets; one that exists but cannot be read or parsed, or a listed secret the keychain cannot read, fails the request or tool result with an error naming the problem instead of running unredacted, and `!` commands are refused until it is fixed.

## Diagnostics and help

- `/help` shows commands and shortcuts (see `clients.md`).
- `/bug` writes the recent log ring and session info to `~/.may/bugs/bug-<unix seconds>.log` and prints the path.
