# Profiles and session organisation

A profile groups sessions and sets where their agents run. Three exist from the start, all on the local machine: Daily (everyday chat; the web UI lands on it; location context is injected), Build (coding; the TUI and one-shot runs use it, and a session created without a profile falls back to it) and Work.

## A profile holds

- Name, icon, display order.
- Environment: `local` (the host) or `container` (agents run in a container; its mounts are listed in the profile preferences with host path, container path and `read_only` or `read_write` access).
- Persona: free text added to the system prompt for sessions in that profile.
- Location awareness: whether the user's inferred city is given to the agent.
- Attached SSH servers, chosen from `ssh_servers` in the config; the agent reaches them through the terminal's `location`. A name no longer configured is skipped with a warning.
- The cloud provider account its cloud machines are created in: the GitHub account for Codespaces, set up with `/cloud` (see `slash-commands.md`). Deleting the profile removes it.
- A pinned limit (10 by default) on pinned sessions plus folders.
- Per-profile files under `~/.may/profiles/<id>/`: `workspace.toml` (approved directories and repository trust), memories, skills and the session directories.

## Managing them

- Web UI: Settings > Profiles, or New profile in the sidebar (name, persona; environment and the other fields in settings). TUI: `/profile` or Ctrl+P to switch; creating and editing is done in the web UI.
- Deleting a profile moves its sessions to the default profile (Build) and deletes its folders. The default profile cannot be deleted.
- Changing profiles re-evaluates remote access (see `remote-access.md`).

## Session organisation

- Pin a session to keep it at the top, optionally inside a folder; pinned sessions are never auto-archived. Pinning past the profile's limit is refused.
- Archive moves a session out of the Recent list. A session idle for longer than `[sessions] recent_hours` (default 48) is archived automatically, checked every 30 seconds, unless it is pinned or active: a running turn, background command or subagent, a pending queued message, or a pending permission or workspace request (an open `ask_user` question does not keep it). Archived sessions are listed in the TUI by `/archive` and in the web UI's Archive page; opening one makes it recent again.
- Rename and delete are in the web UI session menu; delete removes the conversation and its files.
