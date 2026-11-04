---
name: may
description: Facts about how May itself works (terminal, background jobs, daemon, sessions, sandbox, commands, skills, subagents, issues and PRs, memory, search, compaction); read it before guessing about May's behaviour or when something in the harness surprises you.
---

# May

May is a daemon with clients. The daemon runs every agent loop, the task scheduler and search indexing; the TUI, one-shot runs (`may "<prompt>"`) and the web UI attach to it. This file states how the parts behave today. `references/architecture.md` lists where each part lives in the source.

## Terminal

`terminal` runs a command in the session sandbox. `timeout` (seconds, at most 7200) is required; the command and its process tree are killed when it passes.

- Foreground window: a call waits 10 seconds. A command still running then moves to the background and the call returns its task id with the output so far. The 10 seconds is a fixed window, not a sign that something is slow. `background: true` detaches at once.
- A background command reports its completion to you as an event when it ends, and a finished command starts a new turn if the session is idle. Do not wait for it with `sleep` or by re-reading unchanged output; both are refused. Use `terminal_read` for new output, `monitor` to follow a command or file continuously, `terminal_input` to write to stdin, `terminal_kill` to stop a job.
- A job running past 40 minutes raises one long-running alert; check it with `terminal_read`.
- stdin is an open pipe that never reaches end of file. A command that reads stdin blocks on it. The harness watches for this and raises one "waiting for input on stdin" alert per blocking episode, which means the command is reading stdin, not that it is working. Send input with `terminal_input` or kill the job. Tools that read stdin when given no path (`rg`, `grep`, `cat`) block this way when a pipeline feeds them nothing: pass a path or `</dev/null`.
- The working directory persists after a successful `cd`. It is reset to the session directory, with a "Commands now run in ..." alert naming the reason, when the last command failed, ended through `exit` or a signal, ended outside readable paths, or the directory vanished. Detached jobs never carry a directory.
- `~` and `$HOME` are the session directory, in every mode. Use absolute paths for host locations.
- Output over 16,000 bytes is stored under `outputs/` and you receive a head and tail preview with the file path; read the slice you need from the file. Source code may be shown folded and JSON as tab-separated text, only when output is cut. `raw: true` returns the exact bytes. `max_lines` (default 200, at most 800) sets the inline line budget.
- Modes: `default` and `sandboxed` run in the heel sandbox and need no approval; `unsafe` runs on the host and needs the user's approval per script (unanswered requests are denied after 30 minutes for the main agent, 3 minutes for a subagent); `ssh` runs on a configured SSH server.
- Network: sandboxed commands reach the network. The `TerminalArgs` text that says `sandboxed` denies network is out of date: the domain check in `permission.rs` always allows, and only the user's `[preferences.network_policy]` setting (restricted, allow list, deny list) can narrow it.
- Writes are limited to the session directory, directories the user approved (`request_workspace`) and the memories directory. `TMPDIR` is the session's `tmp/`.

## Session directory

Each session has `~/.may/profiles/<profile>/sessions/<id>/`, which is the sandbox and the terminal's starting directory. Session ids are petname words.

- `tmp/` scratch; `files/` uploads and recalled files; `data/` session state; `outputs/` overflow and full command outputs, and `context-dump.md` after a context dump; `artifacts/` deliverables.
- To show a user a file, write it under `artifacts/` and emit `<artifact url="path/inside/artifacts" />` on its own line.
- `tasks.md` is the session's task document. An item waiting on an open `ask_user` question or anything else the run cannot resolve is marked `BLOCKING <request_id or reason>`; a mark naming a request that is answered or withdrawn no longer holds.
- `skills/` links to the profile's skills; `.may/memories` links to the global memories directory; `repos/<slug>` holds the session's private clones of repositories.
- Conversation state lives in the SQLite database `~/.may/history.db` (messages, transcript, context snapshots, runtime state, pending questions and permissions, issues, PRs, scheduled tasks), not in files. After compaction or a restart, recover work from `outputs/`, `data/`, `files/`, `artifacts/` and `tasks.md`.

## Daemon

- Listens on the unix socket `~/.may/daemon.sock` (mode 0600) and on `127.0.0.1:8787` (`[daemon] port`); both trust the local user. A third loopback listener on a random port serves the same routes behind an identity check and is what `tailscale serve` forwards to for remote access. Addresses in `[daemon] listen` are applied live; a non-loopback address requires the peer to be identified through Tailscale (WhoIs), whatever the doc comment on `DaemonConfig.listen` says.
- `~/.may/daemon.lock` keeps it to one instance. Clients start it on demand and replace it when its build id differs, draining running turns first (30 seconds, then the runs are interrupted).
- `may daemon [install|uninstall|status|stop]` manages it; `may rest` runs it API-only; `may ui` needs the `ui` build feature.
- A turn starts with `POST /api/sessions/{id}/messages` and is followed on the `/api/events` stream, which every client shares, with replay of the turn in progress for late subscribers. A client disconnecting does not stop a turn. `POST /api/chat/enqueue` takes mode `steer` (default; interrupts the current turn) or `queue` (runs after it).
- Configuration is `~/.may/config.toml`, applied live without a restart.
- One-shot runs mark the session non-interactive: permission prompts are refused instead of asked.

## Commands

Everything except the native tools (`terminal`, `terminal_read`, `terminal_input`, `terminal_kill`, `monitor`) is a command run inside `terminal`; each has `--help`. Positional arguments map only to required non-list fields; anything else is a `--flag`.

- `ask_user --questions '<json array of {"section","question","option":[...],"multi_select"}>'` works. The positional form shown in older descriptions, `ask_user "Which database?" --option A`, does not parse today (`question` is optional, so it is not positional and the call fails with "unexpected positional argument"); use `--questions`, or `--question "..." --option A --option B`. The call returns a request id at once; the answer arrives later as a message. `withdraw_ask_user <request_id>` closes a question. An unanswered question is not authorization.
- `search <query>` searches code in the session's git worktree by description; flags `--repo`, `--budget`, `--files`, `--path`, `--lang`, `--kind`, `--symbol`, `--wait`. The query `status` reports the index, `index` reconciles it. It is always hybrid (text and vector). Frequently searched repositories are indexed automatically by the daemon.
- `session search|inspect|messages|list` finds and reads past sessions.
- `issue` (create, view, list, search, edit, comment, close, label, blocked-by, sub-issue) on the May backend by default or GitHub when the project is configured so.
- `pr open|merge|list|show|close|adopt` lands work from the session's clone. One open PR per session and repository; `open` merges as soon as checks pass unless `--hold`; a failed check or conflict rejects it, so fix in the clone and `pr merge <n>` again. `repo trust set full|prs-only|local-branch|local` sets how far May may go for a repository.
- `task` creates scheduled tasks (time, event or webhook triggers) that need the user's approval; `notify` sends a push notification; `tunnel` exposes a local port; `browser` drives the May-managed browser; `webfetch` reads a URL (not present when the provider hosts fetching); `ask` asks a smaller model about content; `imagine` generates images; `agent list|send <name> <prompt-file>` messages other main agents on this machine; `register-skills` registers a local skills directory; MCP servers the user connected appear as commands.
- Web search is a tool the model provider runs; there is no `websearch` command.

## Skills

A skill is a directory with `SKILL.md` (frontmatter `name`, `description`) and optional `references/`, `assets/`, `subagents/`. The system prompt lists each skill's name and description; read the `SKILL.md` before acting on a matching request. Bundled skills are compiled into the binary, written to `~/.may/skills/` at startup (overwriting, so local edits to bundled skills are lost), copied into each profile's skills directory, and linked into the session as `skills/`. Skills can also be installed from a GitHub repository's `skills/` directory.

## Subagents

`subagent start --subagent <type or path.md> --prompt-file <path> [--model advanced|balanced|fast] [--worktrees ...] [--commit-worktrees ...]` and `subagent resume --id <n> --prompt-file <path>`. The prompt is always a file you wrote first; inline prompt text is rejected (the `--prompt "<prompt>"` wording in the base system prompt is out of date). Built-in types: `explore`, `plan`, `research`, `fork`. Without `--model`, a small model reads the prompt and picks a tier. A resume id expires with the provider's prompt cache. Subagents cannot use `ask_user` or `agent`; for them `issue`, `pr` and `repo` only read. Write access beyond their own space comes only from `--worktrees` and `--commit-worktrees`, and their commits are listed in the result.

## Repositories, PRs and CI

Work on a git repository happens in a clone inside the session, and `pr` lands it. A session writes the user's checkout only through `pr`.

### Clones

- The first `pr` call in a repository creates the session's clone at `repos/<name>-<8 hex>`, recorded in `repos/repos.toml`. A local repository is cloned with `git clone --local` from the checkout (objects hardlinked, `origin` is the checkout path). A project configured with `pr.backend = "github"` is cloned from its remote URL, with the remote named `upstream`. A repository with no persisted trust decision, or with trust `local`, gets no clone: set trust with `repo trust set full|prs-only|local-branch` first.
- With one clone in the session `pr` uses it wherever the command runs; with several, pass `--repo <repo key or clone directory name>`. Once a clone exists `pr` does not pick the repository from the current directory.
- A clone does not persist. It is deleted when its PR merges (the directory, its entry in `repos.toml` and, for a local clone, the `refs/may/<session>` ref in the checkout) and when the session is deleted. Ignored files such as build output go with it. Persist work by committing it in the clone, or by writing it outside the clone under `artifacts/`, `outputs/`, `data/` or `tmp/`. After a merge the next `pr` call clones again from the new HEAD.
- `pr open` and `pr merge` refuse while the clone has uncommitted changes or untracked files that are not ignored, and list them. Commit them or move them out.

### PRs

- `pr open [--hold] [--issue <n>] [--title <t>] [--repo <key>]` records the PR and, without `--hold`, runs `pr merge` at once and returns when that ends. `pr merge <n>` is also the retry after a failure or rejection. `pr list [--state open|merged|closed|all]`, `pr show <n>` (with the event trail), `pr close <n>` for your own unmerged PR, `pr adopt <n>` to take over a PR whose session is gone or that another session owns.
- One open PR per session and repository. `open` needs committed work in the clone (HEAD different from the base).
- Numbers are per repository. Every transition (opened, checks_pending, merging, queued, merged, rejected, check_failed, closed) is a `pr_events` row with the session id and time. The agent column always reads `main` and `message_id` is empty, so an event does not yet trace back to the message that caused it (May-Agent/may#319).
- Local backend: the project's `checks` command (`[[projects]]`) runs in the clone. Then, serialised per repository, the clone's HEAD is fetched into the checkout as `refs/may/<session>`, `git merge-tree --write-tree` looks for conflicts, a file the user has modified in the checkout that the PR also touches is refused, and the merge is a `--no-ff` merge commit on the checkout's current branch. A conflict or overlap rejects the PR and lists the files: rebase in the clone and run `pr merge <n>` again.
- GitHub backend: the clone pushes `may/<session>` to `upstream`, `gh pr create` opens the PR, `gh pr merge --auto --squash` queues it (so a merge queue enqueues it), and `pr` polls `gh pr view` every 10 seconds for up to 10 minutes. On `MERGED` it runs a best-effort `git pull --ff-only` in the checkout of the launch directory when that checkout's `origin` is the same repository, then deletes the clone. A PR closed upstream, or still open after 10 minutes, ends the call with an error. With `pr.screen = true` on the project, the fast model judges the title first and a flagged title refuses the publish.
- Not implemented, so do not rely on it (May-Agent/may#319, #320, under #134): the GitHub backend never reads `gh pr checks`, so a red check shows only as a PR that does not merge; it is not chosen from `gh auth` and a GitHub remote, the project must name it; pushes are plain `git push` with no `--force-with-lease`; the PR body is empty and only the title is screened; pull-sync covers one checkout, not every recorded one, and there is no `pr.pull_sync` setting; `pr.auto_merge` and `pr.require_review` parse but are not applied, only `--hold` stops the merge; `--issue` records an event and does not close the issue.

### Subagents and repositories

- Subagents cannot push or open PRs. In the subagent command registry (`configure_role_tools` in `src/terminal_commands.rs`) `pr` accepts only `list` and `show`, and `repo` and `issue` only read. The main agent reviews a subagent's work and opens the PR itself.
- A subagent writes only where `subagent start` granted it: `--worktrees` (writable to it, no commits) and `--commit-worktrees` (it may commit there, on a `subagent/<id>/` branch). Its commits are listed in the result; read them with `git log` and `git diff` before bringing them into the clone `pr` lands.
- Not implemented (May-Agent/may#318): a child clone of the session clone at `subagents/<run>` for each subagent run. `SessionRepos::child_clone` in `src/repo_clone.rs` exists but nothing calls it, so grant a worktree instead.
- Not implemented (May-Agent/aither#206): approval of a subagent's `unsafe` command by its parent. Today the request is attributed to the parent's session and goes to the user, who has 3 minutes before it is denied; the parent neither sees nor decides it. A subagent that needs host access should say so in its result so the main agent can run the command.

## Memory

`~/.may/memories/MEMORY.md` is an index you maintain by editing files under that directory; link only to files inside it. Separately, a background extractor refreshes each profile's memories (`preference` and `recency`) at most every 24 hours, and they are placed in the context under `## User Preferences` and `## Recent Context`.

## Context and compaction

There is no `compact` tool you can call. The conversation is compacted into a handoff document in the background when usage passes the context policy's `compact_at` threshold, when the session has been idle until the provider's prompt cache (default one hour, the provider's `cache_lifetime` setting) is about to expire, or when the user runs `/compact`. The idle session's agent is kept between turns and reused unless something it was built from changed, so detached commands keep running. `/context` and `/btw` are answered without disturbing the conversation.

## Defects to know

- `ask_user` positional form fails (above).
- The `subagent` description in the base system prompt names `--prompt`; only `--prompt-file` works.
- The terminal's `sandboxed` mode text claims the network is denied; it is not.
- The "descendants were killed" alert points to a `terminal_background` command that does not exist; use `background: true`.
