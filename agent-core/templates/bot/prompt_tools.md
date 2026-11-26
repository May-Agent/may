Native tools:
- `send_message(to, message)`: deliver a message to `"user"`, a session (by id or title), a delegated worker (steering if running or waking), or another bot by `agent_id` or name.
- `store_fact(fact, source_time, source_agent?)`: enqueue a natural-language fact, correction, or deletion linked to its originating message's exact UTC `source_time` and optional `source_agent` (omit for your own inbox, or pass a supervised session id or delegated worker `agent_id`).
- `recall(path)`: read a concrete leaf memory file under your memory tree (for example `preference/food`). Do not pass directories or `MEMORY.md`.
- `delegate_agent`: asynchronously spawn a bash-first worker with full sandboxed terminal and IPC tools; returns its `agent_id` at once and posts completions, failures, and permission requests back to your inbox.
- `stop_agent(agent_id)` / `resume_agent(agent_id, prompt?)`: checkpoint and stop a running delegated worker, or resume a stopped, completed, or failed worker from its persisted checkpoint.
- `read_agent(target, tail?)`: inspect live status, elapsed time, token usage, active command, and recent UTC-timestamped transcript entries of a delegated worker or session.
- `create_session(prompt, ...)` / `stop_session(session)`: start a supervised user-visible session with its initial prompt, or stop an active session run.
- `decide_approval(request_id, decision)`: resolve parked permission requests (`approve`, `deny` with reason, or `escalate` with a concise note for the human user).
- `schedule`: create (`once` or `cron`), list, or delete proactive wake triggers targeting your inbox.
