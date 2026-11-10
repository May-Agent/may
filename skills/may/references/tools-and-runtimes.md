# Tools and runtimes

## The shell

Terminal commands run in a managed GNU bash 5.3.20 (pinned for macOS arm64, Linux arm64 and Linux x86_64), not in the system `sh` or the user's login shell. It is started as `bash --noprofile --norc -c <script>`: no startup file is read, so a script behaves the same on every machine. Local sandboxed, `unsafe` host, SSH and container runs all use the managed bash (a container image carries the same pinned tools). Windows has no managed bash.

Before a script runs, May refuses it when the script's own shell would run `grep`, `awk` or `find` (the refusal names the replacement: `rg`; `python3` or `jq`; `fd`), or `sed` in any form but the line ranges (`sed -n '2,3p' file`), the forms `aither-sed` accepts; they run the host `sed`. The rule binds only the script's own shell, `time` and `eval` included: a program named by path, run through a wrapper such as `env`, `xargs` or `command`, or by another shell (`sh -c`, a Makefile, a `python3 -c "import subprocess..."`) still reaches the system programs. A script that kills processes by name or pattern, a shell script that does not parse, a foreground `sleep`, and a script that only re-reads files unchanged since their last successful read are refused the same way.

## What May provides

May embeds a hash-pinned tool distribution (`src/tools/tool-distribution-0.24.0.json`, the byte-exact release asset of aither's `patch-v0.24.0`). Artifacts are downloaded on first use from their upstream release URLs and verified against their SHA-256 before anything runs them.

| command | what | version |
| --- | --- | --- |
| `bash` | the shell | 5.3.20 |
| `rg` | ripgrep | 15.2.0 |
| `ast-grep` | syntax-aware search | 0.45.3 |
| `fd` | file finder | 10.5.0 |
| `jq` | JSON | 1.8.2 |
| `apply_patch` | contextual multi-file editing | 0.24.0 |
| `aither-sed` | decides which `sed` forms scripts may use (line ranges); the host `sed` runs them | 0.25.0 |
| `xargs` | uutils findutils | 0.11.0 |
| coreutils (`ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `head`, `sort`, `date`, `env`, `sha256sum`, `stat`, and the rest of the uutils utilities) | uutils coreutils multicall binary, each name a symlink to it; not `grep`, `sed`, `awk`, `find` | 0.12.0 |
| `uv` | Python package and project manager | 0.13.7 |
| `python3` | CPython installed by `uv` when the tools are prepared | 3.13.18 |

Not every platform has every tool: macOS arm64, Linux arm64 and Linux x86_64 get all of them; Windows gets only `rg`, `ast-grep`, `jq` and `apply_patch`. Separately, `src/tools/host-programs.json` pins programs the harness itself runs (the repo-search daemon, `search`, version 0.0.13); they are never put on a command `PATH`.

## Where they live

- `~/.may/tools/artifacts/`: downloaded, hash-addressed artifacts.
- `~/.may/tools/runtimes/<key>/`: the prepared runtime, executables in `<key>/bin`. `<key>` is a SHA-256 over the target platform and the name, version and digest of every tool plus the Python pin, so the whole distribution is one content-addressed directory: a new distribution gets a new directory. It is made read-only, verified on every reuse and rebuilt if its contents are invalid; creation is serialised by a lock file `~/.may/tools/runtimes/.<key>.lock`. Every session and every backend on the machine shares it.
- `~/.may/tools/runtimes/uv/`: the one writable directory beside the runtimes, private to the user, holding `uv`'s cache (`cache/`) and the Pythons `uv` installs for projects (`python/`).

## Why bash and python3 share a runtime directory

The interpreter is installed into the same tree as the tools, under `<key>/bin/.python/<request>/`, and `<key>/bin/python3` is a relative link to it. Keeping it inside `bin` means the single read-only executable grant that every backend already gives the tools directory covers the whole interpreter, and one hash pins the shell, the tools and Python together. Because the tree is read-only, nothing installs into it; writable state goes to the separate `uv/` directory and to the session.

## PATH and environment

A session's `PATH` is the prepared directory `<key>/bin` first, then the host's own `PATH` (so `cargo`, `git`, `gh`, and anything else on the host stay reachable). Managed tools therefore win over a system tool of the same name (the system `python3`, `jq`, `rg`, `ls`, ... are shadowed); a host tool with no managed counterpart is found as usual. The terminal also sets `HOME` and `TMPDIR` to the session directory (`TMPDIR` to its `tmp/`), `XDG_CACHE_HOME` and `XDG_CONFIG_HOME` under it, and `PYTHONPATH` to start with the session's `skills/python`, followed by any inherited value.

Commands a connected ACP agent runs through ACP `terminal/*` get the same `PATH`: the session's managed directory first, then the host's, and the managed directory is readable and executable in their sandbox. The agent's own process keeps the host `PATH`. `may exec` and scheduled commands prepare the same tools as a session's terminal.

## Read policy

A sandboxed command reads everywhere on the machine except credential stores and keys (for example `~/.ssh`, `~/.config/gh` and keychain items); it writes only the session directory and granted workdirs. A command that needs a denied path or the host environment as it is runs as an `unsafe` command with the user's approval.

## Python projects and uv

The environment exports `UV_CACHE_DIR=~/.may/tools/runtimes/uv/cache`, `UV_PYTHON_INSTALL_DIR=~/.may/tools/runtimes/uv/python` and `UV_PYTHON_PREFERENCE=system`. Effects:

- `python3` is always the managed 3.13.18. A project that asks for no particular Python gets that interpreter from `PATH` through `uv` (`system` preference) instead of whichever interpreter `uv` last installed for another project.
- A project's `.python-version` or `requires-python`, or an existing `.venv`, that the `PATH` interpreter does not satisfy is not refused: `uv` installs the requested Python into `UV_PYTHON_INSTALL_DIR` (shared by all sessions) or uses the one the `.venv` was built with. Use `uv venv`, `uv sync`, `uv run` and `uv pip` for project environments.
- The first run of the first session downloads and verifies artifacts and installs Python, so it is slow; later sessions reuse the directory.

## Ids the agent sees

A session is named by two common English words joined by `-` (`amber-river`): that name is the session's id and the id of its main agent (`agent list`, `agent send`, issue owners, `session` results). The daemon issues every name from one generator, records it, and never issues it again, even after its session is deleted; a session keeps its name for life, archived or not.

Every other id the agent reads or types back is one common English word that is a single token (checked against `o200k_base`), from the pool of the session it belongs to: background job ids, stored output files (`outputs/<word>.<ext>`), subagents, `ask_user` questions (`BLOCKING <word>`, `withdraw_ask_user`), tunnels (`/tunnel/<session>/<word>/`), scheduled tasks, browser screenshots (`artifacts/browser/<word>.<ext>`) and `imagine` images. No two live ids of one session share a word whatever they name, and none is ever a session name, since a pool word has no `-`. A word is free again once its thing can no longer be addressed: a job forgotten, a subagent past its cache lifetime, a question answered or withdrawn, a tunnel closed, a task deleted; a screenshot or image keeps its file name, and its word is only held while the file is named. Open questions and scheduled tasks keep their word across daemon restarts; the web UI and the API keep addressing them by their request and task ids.
