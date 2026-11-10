# Configuration

Settings live in `~/.may/config.toml`, a missing file means defaults, and a change applies live without a restart (some sections reach a running session at its next turn). An invalid file or value is rejected with an error naming the problem. The same settings are edited in the TUI `/config` screen and the web UI Settings page. MCP servers are not in this file: they are in `~/.may/mcp.json`.

## Setting up a model

1. Add a provider: `/config` > Providers in the TUI, Settings > Providers in the web UI, or a `[provider.<id>]` table.
2. Pick the model: `/model` in the TUI or the model picker in the web composer for one session; Settings > Models sets the tier lists and default.

Provider ids that match a type name infer the type (also aliases such as `claude`, `gpt`, `gemini`, `github`, `codex`, `lm-studio`, `xai-oauth`); any other id needs `type = "..."` and is `custom` otherwise. Types and default base URLs:

| type | needs | default base URL |
| --- | --- | --- |
| `anthropic` | api_key | https://api.anthropic.com |
| `openai` | api_key | https://api.openai.com/v1 |
| `google` | api_key | https://generativelanguage.googleapis.com/v1beta |
| `copilot` | OAuth sign-in | https://api.githubcopilot.com |
| `chatgpt` | OAuth sign-in (ChatGPT subscription) | https://chatgpt.com/backend-api/codex |
| `openrouter` | api_key | https://openrouter.ai/api/v1 |
| `deepseek` | api_key | https://api.deepseek.com/v1 |
| `ollama` | none | http://localhost:11434/v1 |
| `lmstudio` | none | http://127.0.0.1:1234/v1 |
| `groq` | api_key | https://api.groq.com/openai/v1 |
| `together` | api_key | https://api.together.xyz/v1 |
| `mistral` | api_key | https://api.mistral.ai/v1 |
| `xai` | api_key | https://api.x.ai/v1 |
| `grok` | OAuth sign-in (xAI subscription) | https://api.x.ai/v1 |
| `custom` | `base_url` (api_key optional) | none |

Anthropic and Google use their own wire formats; every other type speaks the OpenAI format. OAuth providers are signed in from the TUI (`/login` for ChatGPT, Grok and OpenRouter; `/config` for Copilot).

`[provider.<id>]` keys: `type`, `api_key`, `base_url`, `display_name`, `enabled` (default true; a disabled provider is unusable), `cache_lifetime` (the prompt-cache lifetime of the main conversation on that provider, such as `"1h"`, `"5m"` or permanent), `api` (custom providers only: `chat-completions` default, or `responses`), `[provider.<id>.native_tools]` with `web_search` and `web_fetch` booleans (hosted provider tools; a custom provider needs `api = "responses"` to use them; hosted fetch removes the `webfetch` command).

`[models]`: `advanced`, `balanced`, `fast`, `image_generation` are lists of `provider::model` selectors in priority order; `compaction_model` is the model compaction runs on instead of the session's (idle compaction is off while it is set). `last_model` and `last_effort` (top level) are written when you choose with `/model` and `/effort`.

## Agent behaviour

- `max_iterations`: iterations per query, 0 (default) is unlimited.
- `[context]`: auto-compaction. The agent pauses to let compaction finish at `min(window - handoff_reserve, compact_ceiling)` tokens; defaults `handoff_reserve = 20000` (tokens kept free for the compaction request and the handoff, whatever the window), `compact_ceiling = 400000`, `start_distance = 50000` (background compaction starts that many tokens before that point), `warn_distance = 80000` (the agent is told once to save its work and compact), `count_distance = 30000` (a model that counts through a provider endpoint is asked for an exact count within that many tokens of the warn point).
- `[language]`: `communication` (language of replies, questions and recaps; unset follows the user's messages) and `working` (language written for others such as issue text; English when unset). Values are an English or native name or an ISO 639-3 code; an unknown name is rejected.
- `[time] timezone`: IANA name; unset follows the host.
- `[suggestions] enabled` (default true): a completed turn schedules a suggested next prompt shown as ghost text; Tab accepts it in the TUI.
- `[ci]`: when a CI job counts as abnormally long, read each time a PR's checks finish. `max_job_minutes` (default 40) is the absolute limit; `baseline_multiple` (default 2.0, above 1) is the multiple of the job's baseline, the median of its last successful base-branch runs. A job past either gets one alert to the agent and one warning to the user. `pr_poll_seconds` (default 60, above 0) is the interval at which the daemon reads the checks of every in-flight GitHub PR, read on every tick; a PR whose checks fail, whose required checks all pass, or whose checks cannot be read from GitHub gives its session one alert for that event (a new commit or a re-run that fails again is a new event), and `pr show` lists each failed check with the error lines of its Actions log.
- `[recap]`: `auto` (default true) gives an idle session a catch-up card, `idle_minutes` (default 30) is the inactivity before it.
- `[sleep] reminder` (default true): see `sleep.md`.
- `[sessions] recent_hours` (default 48): idle time before a session is archived (see `profiles.md`).
- `[preferences]`: `warn_on_model_change`, `show_token_usage`, `show_decode_speed`, `auto_continue_on_quota_reset` (a turn parked on a quota or rate limit resumes at the reset time the provider announces), all default true. `[preferences.network_policy]`: `mode` (`disabled` default, `restricted`, `allow_list`, `deny_list`) and `hosts` (glob patterns for the two list modes). The TUI Preferences tab toggles the first and third.
- `[issue_approval]`: what publishing asks of the user, each `none`, `auto` (prompt approved if unanswered) or `required` (denied if unanswered). Defaults: `external_issue`, `external_pull_request`, `external_comment` = required; `bug` = none; `feature` = auto; `performance` = none. Owned repositories and comments on issues or PRs the user or agent opened are never prompted.
- `[sandbox] command_priority`: `normal` (default), `low` (nice 10) or `lowest` (nice 19, background I/O) for local and container commands; SSH is unaffected.
- `[sandbox] job_pool_size` (default: the machine's available parallelism): the jobserver tokens every session on the machine shares. Local and container commands, subagents, ACP agents and `may exec` draw from it through `MAKEFLAGS`/`CARGO_MAKEFLAGS`, so their builds together never run more jobs than the pool holds; a session with SSH servers draws nothing from it. A change applies at once: growth immediately, a shrink as running commands return their tokens. Set it in the TUI Preferences tab ("Job pool size", empty is automatic) or the web UI Settings execution section.
- `[host_load]`: when the agent is told the host is under memory pressure or load: `memory_threshold` (`warning` default or `critical`), `load_threshold`, and durations `start_after`, `end_after`, `poll_interval`, `high_load_foreground_wait` written as `"30s"`, `"1m"`; defaults come from aither. A change reaches a session at its next turn.
- `[tasks]`: `line_limit` (default 60), the lines an agent's `tasks.md`, a subagent's included, may hold before the agent is asked to rewrite it. A change applies to running sessions at once.
- `[disk]`: free bytes of the home environment's volume below which the agent is told the disk is low: `yellow_free_bytes` (default 10 GiB), `red_free_bytes` (5 GiB) and `final_free_bytes` (3 GiB). A change applies to running sessions at once.

## Connections

- `[[ssh_servers]]` entries `name` and `target` (`user@host[:port]`); adding one probes it with `uname -s -m` and saves the reported `platform` (`os`, `arch`), so an unreachable server is not saved; the session prompt lists each server as `name (target), OS arch`; attach to profiles (see `profiles.md`).
- `[acp_agents.<name>]` (written by `/connect`): `command`, `args`, `env`, `model`, `mode`, `description`, `grants`. The name must be ASCII letters, digits, `-`, `_` and not a built-in subagent name. `[acp] trial_limit` (default `"60s"`) bounds the handshake.
- `[browser]`: `enabled` (default false), `backend` (`managed` or `remote_cdp`, the latter requires `remote_cdp_endpoint`), `port` (39222), `stealth_enabled` (true), screencast tuning (`screencast_jpeg_quality` 80, `screencast_max_width` 1920, `screencast_max_height` 1200, `every_nth_frame` 2, `encoder_initial_bitrate_kbps` 4000) and `system_profile` (`path`, `copy_on_first_use`) to seed the browser from a Chromium profile on first launch.- `[stream] codec`: `auto` (default; hardware H.264 else software AV1), `h264` or `av1` for browser and desktop streams.
- `[computer] signing_identity`: codesign identity for the macOS desktop helper.
- `[daemon]`: `port` (default 8787) and `listen` (extra addresses, applied live). `[remote_access]`: see `remote-access.md`.
- `[appearance]`: `font` (default Inter) and `theme` (default `system`), used by the web UI.

## Screens

- TUI `/config` has three tabs (Tab switches): Providers, Models, Preferences.
- Web Settings sections: Appearance, Language (interface languages English, Traditional Chinese, Japanese), Profiles, Models, Providers, MCP Servers, Skills, Memory, Triggers, Command Priority, Browser, Stream codec, Notifications, Network (recent sandbox allow/deny decisions), Activity (recent LLM requests and cache hit rate), Remote access, Remote servers.
