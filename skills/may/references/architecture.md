# Where May's parts live

Paths are relative to the May source checkout. `aither-sandbox` and `aither-agent` are the framework crates May builds on (version 0.14.1); their sources are in the cargo registry.

## Daemon and clients

- `src/daemon/mod.rs`: `run_daemon` (listeners, single-instance lock `acquire_single_instance`, `SHUTDOWN_GRACE` 30 s, extra `[daemon] listen` listeners in `ExtraListeners::reconcile`).
- `src/daemon/lifecycle.rs`: `ensure_running` (build-id comparison, start timeout 60 s), launchd and systemd install.
- `src/daemon/serve.rs`, `server.rs`, `events.rs` (the `/api/events` stream), `remote_access.rs` (Tailscale serve).
- `src/daemon/chat/mod.rs`: `run_agent_loop`, the one turn runner. `chat/hub.rs` fans turns out; `chat/hosts.rs` keeps each session's idle agent and answers `/compact`, `/context`, `/btw`; `chat/background.rs` tracks terminal jobs and subagent runs.
- `src/client/`: `DaemonClient` and the SSE parser. `src/bin/may/`: `cli.rs` (argument parsing), `main.rs`, `oneshot.rs`.
- `src/config.rs`: `MayConfig` (`daemon`, `preferences`, `provider`, `models`, ...). `ProviderInstance.cache_lifetime` and `src/cache_lifetime.rs` hold the prompt-cache lifetime.

## Sessions and storage

- `src/session/layout.rs`: `MayHomePaths`, `ProfileHomePaths`, `SessionPaths` (every directory name above).
- `src/session/store.rs`: creates a session's sandbox, links `skills` and `.may/memories`, seeds profile skills.
- `src/db/sql/chat_schema.sql` and `src/db/`: the SQLite schema behind `~/.may/history.db`.
- `src/profile/mod.rs`: profiles (`daily`, `build`, `work` built in; one-shot and TUI use `build`).
- `src/repo_clone.rs`: per-session repository clones under `repos/<slug>`.
- `src/workspace.rs`: approved directories; the file is `workspace.toml` in the profile home.

## Terminal and sandbox

- `aither-sandbox/src/terminal.rs`: `TerminalArgs`, `DEFAULT_FOREGROUND_WAIT` (10 s), `DEFAULT_TIMEOUT_CAP`, `DEFAULT_LONG_JOB_ALERT_AFTER` (40 min), session environment (`HOME`, `TMPDIR`).
- `aither-sandbox/src/stdin_watch.rs` (stdin-blocked detection), `poll_guard.rs` (refused `sleep` and unchanged re-reads), `output.rs` (`INLINE_OUTPUT_LIMIT` 16,000 bytes), `monitor.rs`, `builtin/terminal.rs` (`terminal_read`, `terminal_input`, `terminal_kill`), `command.rs` (IPC command bridge and argument parsing).
- `aither-agent/templates/job_alert.txt`: alert wording, including the working-directory reset reasons.
- `src/agent/permission.rs`: approval for `unsafe` mode (30 min main, 3 min subagent); `src/terminal_permission_handler.rs`: network policy and audit log; `src/agent/runtime_context.rs` and `templates/prompt/`: the session system prompt, one template per section (`tag::layout` orders them), external content last.

## Commands, skills, subagents

- `src/terminal_commands.rs`: `register_shared_terminal_commands`, the list of IPC commands.
- Command implementations: `src/agent/ask_user_tool.rs`, `issue_tool.rs`, `pr_tool.rs` (store in `src/pr.rs`), `repo_tool.rs`, `session_tool.rs`, `task_tool.rs`, `notify_tool.rs`, `tunnel_tool.rs`, `browser_tool.rs`, `register_tool.rs`, `imagine.rs`; `src/repo_search/code_search.rs` (`search`); `src/agent_link.rs` (`agent`); `src/issue/` (May and GitHub backends).
- `src/skill.rs`: `SkillManager` (list, install from GitHub, register local). `src/bundle/mod.rs`: bundled skills embedded with `include_dir!` from `skills/` and written to `~/.may/skills`.
- `aither-agent/src/specialized/task.rs`: `subagent` arguments; `aither-agent/subagents/*.toml`: the bundled definitions, written to `~/.may/subagents/`.
- `src/memory.rs`: profile memory extraction; `templates/prompt/work.md`: the memory directory rule.
- `src/agent/may.rs`: builds the agent (skills, compaction model, hooks, host load monitor); `templates/prompt/capabilities.md`: the command, skill and subagent catalog.

## Repositories, PRs and CI

- `src/repo_clone.rs`: `SessionRepos` (`ensure_local_clone`, `ensure_remote_clone`, `delete_clone`, `delete_all`, unused `child_clone`), `uncommitted_changes` (the dirty-clone check), the `repos/repos.toml` registry.
- `src/pr.rs`: `PrStore` (`resolve` clone choice and trust gate, `open`, `merge`, `merge_github`, `pull_sync`, `screen_outbound`), `land` (fetch `refs/may/<session>`, `merge-tree` preflight, overlap check, `--no-ff` merge), `gh` wrapper, `screen_with_llm`.
- `src/agent/pr_tool.rs`: `PrTool` (all subcommands) and `SubagentPrTool` (`list`, `show`); `src/agent/repo_tool.rs`: `repo trust`; `src/trust.rs`: trust levels.
- `src/terminal_commands.rs`: `PrContext` construction and `configure_role_tools(pr_tool, subagent_pr_tool)`.
- `src/workspace.rs`: `ProjectConfig.checks` and `ProjectPr` (`backend`, `screen`, `auto_merge`, `require_review`).
- `src/db/pr.rs` and `prs` / `pr_events` in `src/db/sql/chat_schema.sql`: PR rows and the event trail.
- `src/agent/permission.rs`: `for_subagent` and `SUBAGENT_APPROVAL_TIMEOUT` (no parent-approval step). `aither-agent/src/specialized/task.rs`: `worktrees` and `commit_worktrees` grants.

## Search

- `src/repo_search/`: `RepoSearch` client of the repo-search daemon, `indexer.rs` (outbox-driven indexing of sessions and issues), `auto_index.rs` (frequently searched worktrees).
