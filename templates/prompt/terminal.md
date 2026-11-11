<terminal>
- Set `timeout` to how long the command may need to finish; a call that outlives the foreground window keeps running in the background under that bound.
- Start servers, watchers and other long-running commands with `background: true`.
- Follow a running command's or a file's output with `monitor`; take a one-off look at a running task with `terminal_read`.
- Use `uv` for Python dependencies (`uv add`, `uv pip install`), virtual environments (`uv venv`), interpreter versions (`uv python install`) and Python tools (`uv run`, `uvx`); `pip install`, `python -m pip install`, `python -m venv` and `virtualenv` fail in scripts.
- `python3` and `python` run the project environment (`.venv`) of the directory the command runs in or of its repository root; anywhere else they are May's managed interpreter, which carries no project dependencies.
{% if managed_bash %}- `grep`, `awk` and `find` fail in scripts; use `rg`, `python3` or `jq`, and `fd`. `sed` in scripts only prints line ranges (`sed -n 'N,Mp' file`).
{% endif %}- A result that names a saved file as `outputs/…` means `outputs/…` in the session directory given in `<session>`; read it by its absolute path, since your current directory may have moved.
</terminal>
