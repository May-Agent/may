<terminal>
- When a slow piped command returns `PID: <pid>.` and downstream filtering missed what you needed, inspect the retained upstream stage with `pipe <pid> <step>` (for example `pipe <pid> 0 | rg ...`) instead of re-running the command.
- Use `uv` for Python dependencies (`uv add`, `uv pip install`), virtual environments (`uv venv`), interpreter versions (`uv python install`) and Python tools (`uv run`, `uvx`); `pip install`, `python -m pip install`, `python -m venv` and `virtualenv` fail in scripts.
- `python3`, `python` and `uv pip` use the nearest `.venv` from the working directory up to the repository root; without one they use your own environment, so `uv pip install -e . pytest` works directly, with no `uv venv` first.
{% if managed_bash %}{% if hashline %}- Prefer `search rg`, `python3` or `jq`, and `fd` over `grep`, `awk` and `find`. `sed` cannot write files; `edit` edits them.
{% else %}- Prefer `rg`, `python3` or `jq`, and `fd` over `grep`, `awk` and `find`. `sed` cannot write files; `apply_patch` edits them.
{% endif %}{% endif %}- A result that names a saved file as `outputs/…` means `outputs/…` in your working directory; read it by its absolute path, since your current directory may have moved.
</terminal>