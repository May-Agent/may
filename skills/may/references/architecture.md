# Where May's parts live

Paths are relative to the May source checkout. `aither-sandbox` and `aither-agent` are the framework crates May builds on (version 0.25.0); their sources are in the cargo registry.

## Daemon and clients

- `src/daemon/mod.rs`: `run_daemon` (listeners, single-instance lock `acquire_single_instance`, `SHUTDOWN_GRACE` 30 s, extra `[daemon] listen` listeners in `ExtraListeners::reconcile`).
- `src/daemon/process_session.rs`: the daemon leads a process session of its own (a daemon that leads a process group, under launchd or in a shell's foreground, runs itself as a child in one and supervises it); `end_exited_session` ends what an unexpectedly exited daemon left in its session. `src/db/daemon_exit.rs` records each start and graceful stop (`daemon_runs`) and the sessions with background work; `chat::restart_interrupted_sessions` restarts warm interrupted sessions with the `reminder_daemon_exit.txt` alert, which a cold one gets with its next turn.
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
- `src/session/binding.rs`: `RepoBinding`, the session's repository binding stored in `sessions.repo` (`github:owner/repo` or `local:<repository identity>`; none for a repo-less chat), and the grouping helpers clients share. `src/repo_gate.rs`: `RepoGate`, run before a session's first turn by `gate_session_repo` in `src/daemon/chat/mod.rs` (asks the trust of a GitHub-bound session with no checkout, runs the checkout preflight in `src/repo_preflight.rs`, then binds the session).
- `src/workspace.rs`: approved directories; the file is `workspace.toml` in the profile home.

## Terminal and sandbox

- `aither-sandbox/src/terminal.rs`: `TerminalArgs`, `DEFAULT_FOREGROUND_WAIT` (10 s), `DEFAULT_TIMEOUT_CAP`, `DEFAULT_LONG_JOB_ALERT_AFTER` (40 min), session environment (`HOME`, `TMPDIR`).
- `src/command_reading/`: `CommandReading`, a terminal script parsed with `brush-parser` into steps (recognised intents or `Unknown` with the source); `patch.rs` reads `apply_patch` text with `aither-patch` and, given the working directory (`CommandReading::of_in`), places hunks with `aither_patch::locate_hunks` for absolute line numbers. The daemon attaches it to tool starts (`daemon/tool_summary.rs`, `daemon/chat/turn_events.rs`) and permission requests.
- `src/command_policy/`: the scripts refused before they run (kills by name, unparsable shell, a foreground `sleep`, unchanged re-reads, host `grep`/`awk`/`find` and unsupported `sed`); `src/tasks_md/`: the `tasks.md` format (rows are a title line plus detail lines), its schema, stop rule and line limit, and `edit.rs`, the `tasks` command (`src/bin/tasks`, installed beside `may`, in the container image at `/usr/local/bin/tasks`) that edits `$MAY_TASKS` with the same parser; `src/agent/notices.rs`: May's wording of aither's alerts.
- `aither-sandbox/src/stdin_watch.rs` (stdin-blocked detection), `output.rs` (`INLINE_OUTPUT_LIMIT` 16,000 bytes), `monitor.rs`, `builtin/terminal.rs` (`terminal_read`, `terminal_input`, `terminal_kill`), `command.rs` (IPC command bridge and argument parsing).
- `aither-agent/templates/job_alert.txt`: alert wording, including the working-directory reset reasons.
- `src/agent/permission.rs`: approval for `unsafe` mode (30 min main, 3 min subagent); `src/terminal_permission_handler.rs`: network policy and audit log; `src/agent/runtime_context.rs` and `templates/prompt/`: the session system prompt, one template per section (`tag::layout` orders them), external content last.

## Commands, skills, subagents

- `src/terminal_commands.rs`: `register_shared_terminal_commands`, the list of IPC commands.
- Command implementations: `src/agent/ask_user_tool.rs`, `issue_tool.rs`, `pr_tool.rs` (store in `src/pr.rs`), `repo_tool.rs`, `session_tool.rs`, `task_tool.rs`, `tunnel_tool.rs`, `browser_tool.rs`, `register_tool.rs`, `imagine.rs`, `explore_tool.rs` (`explore`: the bundled read-only `explore` subagent on the fast model); `src/repo_search/code_search.rs` (`search`); `src/agent_link.rs` (`agent`); `src/issue/` (May and GitHub backends).
- `src/skill.rs`: `SkillManager` (list, install from GitHub, register local). `src/bundle/mod.rs`: bundled skills embedded with `include_dir!` from `skills/` and written to `~/.may/skills`.
- `aither-agent/src/specialized/task.rs`: `subagent` arguments; `aither-agent/subagents/*.toml`: the bundled definitions, written to `~/.may/subagents/`.
- `src/memory.rs`: profile memory extraction; `templates/prompt/work.md`: the memory directory rule.
- `src/agent/may.rs`: builds the agent (skills, compaction model, hooks, host load monitor); `templates/prompt/capabilities.md`: the command, skill and subagent catalog.

## Repositories, PRs and CI

- `src/repo_clone.rs`: `SessionRepos` (`ensure_local_clone`, `ensure_remote_clone` — cloned in host scratch, then moved into `repos/` — `delete_clone`, `delete_all`, unused `child_clone`), `CloneSandbox` (runs scripts on the session terminal's sandbox), `uncommitted_changes` (the dirty-clone check, run in that sandbox), the `repos/repos.toml` registry.
- `src/pr.rs`: `PrStore` (`resolve` clone choice and trust gate, `open`, `merge`, `merge_github`, `pull_sync`, `screen_outbound`), `check_pinned` (the project check on a throwaway checkout in the session's `tmp/`, in the sandbox), `push_pinned` (pushes from a bare repository in host scratch that fetched the commit), `land` (fetch `refs/may/<session>`, `merge-tree` preflight, overlap check of the checkout's dirty files against the PR's own work, `base..sha`, `--no-ff` merge), `gh` wrapper, `screen_with_llm`.
- `src/agent/pr_tool.rs`: `PrCommand`, which builds the terminal-bound `PrTool` for each terminal (all subcommands for the main agent, `list` and `show` for a subagent); `src/agent/repo_tool.rs`: `repo trust`; `src/trust.rs`: trust levels.
- `src/terminal_commands.rs`: `PrContext` construction and `configure_terminal_bound_tool("pr", …)`.
- `src/workspace.rs`: `ProjectConfig.checks` and `ProjectPr` (`backend`, `screen`, `auto_merge`, `require_review`, `pull_sync`). A `[[projects]]` entry names a checkout by `path` or, for a GitHub repository trusted with no checkout, by `repository = "owner/repo"`. `pr.pull_sync = false` turns off the post-merge `git pull --ff-only` of every local checkout recorded for the repository (default on; failures are reported in the merge result and logged, never blocking): `src/pr/pull_sync.rs`.
- A GitHub-bound session clones the remote and reads no local checkout; a local-bound one clones its checkout, and a clone based on the checkout's work in progress records the base commit in `session_repo_clones.base_sha`.
- `src/db/pr.rs` and `prs` / `pr_events` in `src/db/sql/chat_schema.sql`: PR rows and the event trail.
- `src/ci/`: CI durations. `record.rs` (`Recorder`: GitHub jobs and steps through `github.rs`, baselines, local checks, queue wait, the finding), `timing.rs` (median, p90, trend, the two thresholds), `logs.rs` (cargo test, nextest and bun test log parsers), `report.rs` (the alert, warning and `ci` templates in `templates/ci_*.txt`). `pr/merge_watch.rs` `Observing` fires the recorder when a PR's checks finish. Tables `ci_jobs`, `ci_steps`, `ci_tests`, `ci_baselines`, `ci_pr_runs`, `ci_findings` in `src/db/ci.rs`. `src/agent/ci_alert.rs` hands a finding to the agent once; `src/agent/ci_tool.rs` is the `ci` command; the warning rides `/api/events` as `session_warning` (`daemon/events.rs`). The `ci-optimise` skill is in `skills/ci-optimise/`. Live PR checks: `pr/checks.rs` (`ChecksReport`, per-check state, the `pr show` section and alert templates `templates/pr_checks*.txt`), `daemon/pr_checks.rs` (the poller: keeps each in-flight GitHub PR's newest read for `GET /api/sessions/{id}/prs`, pushes `prs.changed`, and stores one alert per rollup change through `pr_check_state` / `pr_check_alerts` in `db/pr_checks.rs`, handed to the agent by `agent/ci_alert.rs`).
- `src/agent/permission.rs`: `for_subagent` and `SUBAGENT_APPROVAL_TIMEOUT` (no parent-approval step). `aither-agent/src/specialized/task.rs`: `worktrees` and `commit_worktrees` grants.

## Search

- `src/repo_search/`: `RepoSearch` client of the repo-search daemon, `indexer.rs` (outbox-driven indexing of sessions and issues), `auto_index.rs` (frequently searched worktrees).
