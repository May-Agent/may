# May Bots

May Bots are resident, always-on personal bridge agents hosted by the daemon alongside ordinary bash-first sessions. Multiple bots run concurrently, each with its own persistent SQLite state, stable `word-word-word` `agent_id` in `AgentDirectory`, git-backed hierarchical Markdown memory tree, and event-driven inbox loop.

## Storage and layout

- On disk: `~/.may/bots/<bot_id>/`
  - `memory/`: live hierarchical Markdown tree (`MEMORY.md` in root and every subdirectory summarizing direct children; concrete facts in leaf `.md` files).
  - `memory.staging/`: isolated staging working tree edited by the confined curator before invariant verification and atomic swap.
  - `memory.git/`: separated bare git repository (`--git-dir`/`--work-tree`) recording 1-to-1 provenance commits for every `store_fact`.
  - `delegates/<agent_id>/`: isolated working directory per delegated bash-first worker.
- In `~/.may/history.db`: `bots`, `bot_inbox`, `bot_chat_messages`, `bot_trace_entries`, `bot_context_snapshots`, `bot_delegates`, `bot_delegate_messages`, `bot_supervised_sessions`, `bot_approvals`, `bot_schedules`, and `bot_store_fact_queue`.

## Event-driven inbox and timestamps

- Every inbound event (`user_message`, `agent_message`, `delegate_completed`, `delegate_failed`, `approval_request`, `schedule_wake`, `task_wake`, `webhook_wake`) is appended to `bot_inbox` with a strictly monotonic per-bot UTC enqueue timestamp `ts_ms` (`UNIQUE(bot_id, ts_ms)`): whole-second aligned (`YYYY-MM-DDTHH:MM:SSZ`) when `now_sec * 1000 > last_ts_ms`, advancing by `+1ms` (`YYYY-MM-DDTHH:MM:SS.sssZ`) on same-second bursts or clock step-back.
- Inbox batches render enqueue-time UTC stamps directly via Askama templates without drain-time re-stamping. Newly arrived inbox entries during an active run trigger `Hooks::on_turn_boundary` (`EndTurn`) so thinking blocks remain intact while the new entries drain before the next model request.
- Plain assistant text stays in internal run trace (`bot_trace_entries`) and is never shown as user chat. Only explicit `send_message(to="user", ...)` appends to the user conversation and dispatches push notifications through `NotificationService` (honoring `SleepState`). A `Moment::FinalTurn` guard prompts once if an active run consumed a direct `UserMessage` without calling `send_message(to="user")`.

## Non-bash-first native toolset

Bots never register `terminal` or `monitor`. Their narrow native toolset consists solely of:

- `send_message(to, message)`: `"user"`, a session (by id or title), a delegated worker (steering if running or resuming from checkpoint), or another bot.
- `store_fact(fact, source_time, source_agent?)`: validates `(source_agent, source_time)` against `bot_inbox`, `bot_delegate_messages`, or a supervised session's `messages` (failing fast on unknown or unauthorized citations) and enqueues onto the per-bot serial FIFO curator queue.
- `recall(path)`: reads a concrete leaf `.md` file under `memory/` (`preference/food` normalizes to `preference/food.md`), rejecting empty, directory, traversal, and `MEMORY.md` paths.
- `delegate_agent` / `stop_agent` / `resume_agent`: asynchronously spawns, checkpoints/stops, and resumes bash-first delegated workers under a dedicated **delegated-worker authority level** (top-level workers can request workspace access, SSH, mutate issues, and open/push PRs, routing all four gates `unsafe`, `ssh`, `publish`, `workspace` to the owning bot's `decide_approval`).
- `read_agent(target, tail?)`: inspects live status, elapsed time, token usage, active command, and UTC-timestamped transcript entries of a delegated worker or session.
- `create_session(prompt, ...)` / `stop_session(session)`: creates a user-visible supervised session, records its supervising `bot_id`, enqueues the required initial `prompt`, and routes its four permission gates through the bot's approval router.
- `decide_approval(request_id, decision)`: `approve`, `deny { reason }`, or `escalate { note }`. Under `bot_triage`, requesters park under `ApprovalPause` with no premature `UserWait` or push notification until the bot explicitly calls `escalate { note }`. Per-gate policies (`auto_approve`, `bot_triage`, `always_escalate`) update live without restart.
- `schedule`: creates (`once` or `cron`), lists, and deletes proactive wakes targeting `bot_inbox`. `TaskScheduler` `wake` commands and webhooks targeting a bot's id/name also route into `bot_inbox`.

## Hierarchical memory, curator, and hybrid RAG

- All `MEMORY.md` files render into a resident `ExternalSlot` (`tag::BOT_MEMORY_INDEX`) updated via `Context::update_live` (`LiveContent`) so warm prompt caches stay intact until reassembly.
- `store_fact` runs serially in FIFO order (`1 store_fact = 1 curator run = 1 memory.git commit`) inside a confined bash-first curator over `memory.staging/` (no network, no `unsafe`), verified by Rust structural invariants (`verify_memory_tree`) with warm retry feedback before committing and atomically publishing to `memory/`.
- Leaf `.md` files (never `MEMORY.md`) sync incrementally into `repo-search` collection `may-bot-memory-<bot_id>` (`WireMode::Hybrid`). `<memory-recall>` injects at turn tail and deduplicates against `SeenMemoryLedger` (`blake3` hashes persisted with `bot_context_snapshots`), suppressing duplicates in the uncompacted window, re-injecting with `[UPDATED]` when a leaf's hash changes, and resetting on compaction.

## API and clients

- REST endpoints under `/api/bots/*` (CRUD, live config/policy, messages, trace, inbox, delegates, approvals, schedules, memory tree & `memory.git` history).
- All server-pushed bot updates multiplex onto the single `/api/events` stream under topic `bots` as `bot_state`, `bot_chat`, `bot_trace`, `bot_approval`, and `bot_memory`.
- TUI: `/bot` opens the bot management, chat, trace, approvals, and memory overlay.
- Web UI: `/bots` provides bot management, chat, internal trace drawer, escalated approval actions, live policy controls, and memory tree/history views.
