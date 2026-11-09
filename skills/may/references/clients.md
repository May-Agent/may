# Clients

Every client is a front end of the one daemon; a turn started in one is visible in all, and closing a client does not stop it.

## Command line (`may`)

- `may` or `may chat`: the TUI. `may --repo <owner/repo or path>` binds the session to a repository. `may resume [session-id]` reopens a session (the most recent without an id).
- `may "<prompt>"`, optionally `--model <id>` and `--reasoning-effort <low|medium|high>`: one-shot run; assistant text goes to stdout, process events to stderr. Permission prompts are refused rather than asked. One-shot runs and the TUI use the Build profile.
- `may ui`: starts the daemon if needed and opens the web UI (only in a build with the `ui` feature). `may rest [--host <addr>] [--port <port>]` runs the daemon in the foreground, API only.
- `may daemon [install|uninstall|status|stop]`: run, install as a login service (launchd or systemd), report or stop the daemon.
- `may browser <status|open|close|targets|contexts|cdp-endpoint|read <url> [--detail brief|standard|full] [--screenshot off|viewport|full-page]|provision|kill>` inspects the managed browser; `may exec --session <id> [...] <command>` runs a command in a session's home environment.

## TUI

Typing sends: Enter sends the line. While the agent is working, Enter steers (interrupts the turn with the new message) and Tab queues the message to run after the turn. Tab also accepts a shown prompt suggestion. A turn stopped before any output puts the sent text back in the input.

- Layout: the chat and the input share a two-column side margin. The input has no `>` prompt and starts flush left; a leading `!` gets the bash-mode highlight instead. The input, the status line (`cwd · model · tok/s · index`, context size on the right) and each user message in the chat sit on the same full-width band shade (read from the terminal at startup; the terminal's default background when it does not answer), with one padding row above and below the band, one blank row between input and status, and one plain row between the chat and the band. Queued-message lines stay above the band.
- `/` opens the slash palette (see `slash-commands.md`); `@` opens file-mention completion, inserting the path of a file from the working directory or sandbox.
- `! command` runs a bash command in May's environment; its masked output is returned to the agent as the next message. Never type secrets there; use `/secret`.
- Ctrl+V pastes (an image becomes an attachment); a multi-line or large paste becomes an attachment file.
- Ctrl+B shows or hides the sidebar (tasks, issues, subagents, jobs; only on terminals at least 120 columns wide). Issue rows carry GitHub-style state icons: a green ● for open, a dark ✓ for closed. PR rows show the live check rollup from GitHub: a green ● while the read is pending, yellow ● with checks running, red ● on a failure, and a green ✓ once all required checks pass (green ● while the PR sits in the merge queue). Ctrl+N new session, Ctrl+P profile or session list, Ctrl+Left open questions, Ctrl+C clear / twice quit, Ctrl+Q quit. Up/Down browse input history or scroll the chat, PageUp/PageDown and the mouse wheel scroll, g/G jump to top/bottom, Esc clears the input, Esc twice (idle) opens rewind. Selected text is copied with Ctrl+C.
- A warning for the user, such as an abnormally long CI job, appears in the stream with a warning icon in yellow; it is not part of the agent's context. The web UI shows it with an amber warning icon.
- Dialogs appear for approvals: `unsafe` command permission, workspace access outside the sandbox, repository trust, `ask_user` questions, and a quit confirmation when a run is active. Tool calls can be opened for detail and images viewed full screen.
- Markdown images in assistant replies and image artifacts (`<artifact url="…png"/>`) render inline on terminals with image support (Kitty, iTerm2, Sixel); while an image is fetched or after a failed fetch its reserved rows show a `▏image · caption` placeholder. On terminals without image support they degrade to a link.

## Web UI

Served by the daemon at `http://127.0.0.1:8787` (or the remote address, see `remote-access.md`) when built with the `ui` feature. Routes: `/p/<profile>` (profile home), `/p/<profile>/s/<session>` (a chat), `/p/<profile>/archive`, `/settings`. A 401 from the daemon shows a sign-in-required page.

- Sidebar: profile switcher and New profile; New chat (and New chat in a GitHub repo, with a searchable repository list that needs `gh auth login`); sessions grouped Pinned (with folders), Recent and Archived; per-session actions Pin/Unpin, Move to folder, Archive/Unarchive, Rename, Delete; notifications bell; Settings.
- Shortcuts: Cmd/Ctrl+N new chat (Cmd/Ctrl+Shift+O also), Cmd/Ctrl+K command palette (sessions, new chat, settings, theme, language), Ctrl+Cmd+S (Mac) or Ctrl+Alt+S show/hide sidebar, Option+Cmd+0 (Mac) or Ctrl+Alt+0 show/hide inspector.
- Composer: Enter sends, Shift+Enter newline, attach files, `!command` bash mode, a Queue toggle (queue until the turn ends instead of interrupting), a model picker, a thinking-effort picker, and a stop button. Only `/sleep` and `/sleep off` are slash commands here.
- Chat: streaming reply with decode speed (`show_decode_speed`), collapsible reasoning ("Thought for ..."), tool cards, copy buttons, translation of replies into the interface language with a show-original toggle, a context ring (percent of the window used), banners when held on a quota limit, provider retry or network wait, and a plan banner (plan off / draft / approved, with Approve, Discard and Edit of `tasks.md`). The task sidebar (TUI and web) shows each `tasks.md` row's title only. A yolo switch sits above the composer. Permission, workspace and `ask_user` requests open as modals (secret questions take a value that May never sees).
- Inspector (side pane): Details (rename, tasks with progress, linked issues, subagents with stop, background jobs with stop), Library (files May made, also the Files drawer), Browser and Computer (live view of the agent's managed browser or session desktop; Take over pauses May until you hand back).
- Settings: see `configuration.md`. Interface languages: English (US), Traditional Chinese, Japanese, picked from the browser's language on first load. Themes Light, Dark, System.
