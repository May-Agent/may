# Tools and runtimes

## The shell

Terminal commands run in a managed GNU bash 5.3.20 (pinned for macOS arm64, Linux arm64 and Linux x86_64), not in the system `sh` or the user's login shell. It is started as `bash --noprofile --norc -c <script>`: no startup file is read, so a script behaves the same on every machine. Local sandboxed, `unsafe` host, SSH and container runs all use the managed bash (a container image carries the same pinned tools). Windows has no managed bash.

Before a script runs, May refuses it when the script's own shell would run `grep`, `awk` or `find` (the refusal names the replacement: `rg`; `python3` or `jq`; `fd`). `sed` is not refused by the policy: it is May's own, which runs every form that writes no file and refuses the others itself. The rule binds only the script's own shell, `time` and `eval` included: a program named by path, run through a wrapper such as `env`, `xargs` or `command`, or by another shell (`sh -c`, a Makefile, a `python3 -c "import subprocess..."`) still reaches the system programs. A script that kills processes by name or pattern, a shell script that does not parse, a foreground `sleep`, and a script that does nothing but repeat a read that last succeeded — the same commands with the same words, started in the same directory, of files unchanged since — are refused the same way. Only file reads (`cat`, `head`, `tail`, `sed -n`, `nl`) and `rg` searches of their piped output make a read; a script with any other command (a `cd`, a generator, `sh`), a wrapper, or a redirect to a file is never refused as one, and reading another part of a file, or piping it into another search, is a new read. So is a script whose `apply_patch` patch has a hunk that removes and adds the same code apart from whitespace that carries no meaning in the file's language (spacing or line-wrapping done by hand): the refusal tells the agent to run the project's formatter instead. Whitespace inside a string literal carries meaning, so does a line's indentation in Python, YAML, Haskell, Haml and Sass, Makefiles, Markdown, reStructuredText and diffs, and so does whitespace that keeps two words apart; a file of a language May's syntax highlighting (syntect's bundled syntaxes) does not know, such as TypeScript or TOML, is not checked. The hunk is lexed where it applies in the file, so a string opened above the hunk is still a string inside it; a hunk whose file cannot be read or that does not place in it is not checked either. A hunk that changes anything else, and a hunk that only adds or only removes lines, is not refused.

Command output has each run of blank lines collapsed before the agent reads it. A script whose every step is a file read (`cat`, `head`, `tail`, `sed -n`, `nl`, as `command_reading` recognises them) is the exception: its output keeps every blank line, so the agent reads the file as it is. A script that mixes a read with any other command, a pipeline into another program, or a script that does not parse is command output and collapses.

## What May provides

May embeds a hash-pinned tool distribution (`src/tools/tool-distribution-0.24.0.json`, the byte-exact release asset of aither's `patch-v0.24.0`, with `apply_patch` and `aither-sed` taken instead from May's own `patch/v<version>` and `sed/v<version>` releases, pinned in `src/tools/tool-artifacts.json`). Artifacts are downloaded on first use from their upstream release URLs and verified against their SHA-256 before anything runs them. The agent reads the prepared tools as a list of command names (no versions; `aither-sed` appears as `sed`), and the `terminal` tool description sends it to `rg`, `head` and `tail`, or `ask` for a summary, for saved output, since the managed shell refuses `grep`.

| command | what | version |
| --- | --- | --- |
| `bash` | the shell | 5.3.20 |
| `rg` | ripgrep | 15.2.0 |
| `ast-grep` | syntax-aware search | 0.45.3 |
| `fd` | file finder | 10.5.0 |
| `jq` | JSON | 1.8.2 |
| `apply_patch` | contextual multi-file editing (`may-patch`) | 0.1.0 |
| `aither-sed` | the `may-sed` program, the shell's `sed` (`bin/sed` links to it): uutils' sed in sandbox mode, which runs every script that writes no file and refuses `-i`, `w`, `W`, `s///w`, `e` and `s///e` with a message naming `apply_patch` | 0.2.0 |
| `xargs` | uutils findutils | 0.11.0 |
| coreutils (`ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `head`, `sort`, `date`, `env`, `sha256sum`, `stat`, and the rest of the uutils utilities) | uutils coreutils multicall binary, each name a symlink to it; not `grep`, `sed`, `awk`, `find` | 0.12.0 |
| `uv` | Python package and project manager | 0.13.7 |
| `python3` | CPython installed by `uv` when the tools are prepared | 3.13.18 |

Not every platform has every tool: macOS arm64, Linux arm64 and Linux x86_64 get all of them; Windows gets only `rg`, `ast-grep`, `jq` and `apply_patch`. Separately, `src/tools/host-programs.json` pins programs the harness itself runs (the repo-search daemon, `search`, version 0.0.14, and on macOS arm64 the `podman` client of the managed container runtime, which is downloaded on first use, hash-verified and re-verified before every start); they are never put on a command `PATH`.

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

- `python3`, `python` and `uv` are shims, in a directory of the session that leads the `PATH`, that resolve when a command runs: the interpreter of the project environment (`$UV_PROJECT_ENVIRONMENT` when set, else `.venv`) found in the command's working directory or a parent up to the repository root (the nearest directory with a `.git`; without a repository only the working directory), otherwise the session environment, `.python-env` in the session directory, which `uv venv --system-site-packages` creates from the managed 3.13.18 (so it also sees the interpreter's own packages, such as those a container image preinstalls) the first time `python3`, `python`, `uv pip` or `uv run` needs it (commands racing to create it build separate directories and one is published by an atomic symlink; the others are discarded). It is deleted with the session directory. `uv` is the next `uv` on the `PATH`; where no project environment is found (and `VIRTUAL_ENV` is not already set) `uv pip` and `uv run` get `VIRTUAL_ENV` set to the session environment, so `uv pip install <package>` then `python3 -c 'import <package>'` works outside a project. `VIRTUAL_ENV` is never set for the whole session because it outranks a project's `.venv` for `uv pip`; inside a project `uv` runs unchanged. Container sessions behave the same: the workspace image carries the same template as `python3`, `python` and `uv` in `/opt/may/shims`, first on the image's `PATH`, in front of the image's own `python3` (the system Python) and `uv`; there the session directory is the mounted `/workspace` (`HOME`), so `.python-env` lives in the session workspace. The template is staged into the image build context and is a hash input of the image tag. The shim is rewritten when its template changes. The sandbox must be able to read the project's environment, which it can wherever the project directory is granted. A project that asks for no particular Python gets that interpreter from `PATH` through `uv` (`system` preference) instead of whichever interpreter `uv` last installed for another project.
- A project's `.python-version` or `requires-python`, or an existing `.venv`, that the `PATH` interpreter does not satisfy is not refused: `uv` installs the requested Python into `UV_PYTHON_INSTALL_DIR` (shared by all sessions) or uses the one the `.venv` was built with. Use `uv venv`, `uv sync`, `uv run` and `uv pip` for project environments.
- The agent must use `uv` for Python dependencies (`uv add`, `uv pip install`), virtual environments (`uv venv`), interpreter versions (`uv python install`) and Python tools (`uv run`, `uvx`); the system prompt says so. The command policy refuses, before they run, `pip install` and `pip3 install` (also as `python -m pip install` or `python3 -m pip install`, and through any path to the program), `python -m venv` and `virtualenv`, and the refusal names the `uv` command that does the same job. `uv` itself and read-only pip subcommands (`pip list`, `pip show`, `pip freeze`) are not affected.
- The first run of the first session downloads and verifies artifacts and installs Python, so it is slow; later sessions reuse the directory.

## Ids the agent sees

A session is named by two common English words joined by `-` (`amber-river`): that name is the session's id and the id of its main agent (`agent list`, `agent send`, issue owners, `session` results). The daemon issues every name from one generator, records it, and never issues it again, even after its session is deleted; a session keeps its name for life, archived or not.

Every other id the agent reads or types back is one common English word that is a single token (checked against `o200k_base`), from the pool of the session it belongs to: background job ids, stored output files (`outputs/<word>.<ext>`), subagents, `ask_user` questions (`BLOCKING <word>`, `withdraw_ask_user`), tunnels (`/tunnel/<session>/<word>/`), scheduled tasks, browser screenshots (`artifacts/browser/<word>.<ext>`) and `imagine` images. No two live ids of one session share a word whatever they name, and none is ever a session name, since a pool word has no `-`. A word is free again once its thing can no longer be addressed: a job forgotten, a subagent past its cache lifetime, a question answered or withdrawn, a tunnel closed, a task deleted; a screenshot or image keeps its file name, and its word is only held while the file is named. Open questions and scheduled tasks keep their word across daemon restarts; the web UI and the API keep addressing them by their request and task ids.
