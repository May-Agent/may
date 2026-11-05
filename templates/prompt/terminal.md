<terminal>
- Set `timeout` to how long the command may need to finish; a call that outlives the foreground window keeps running in the background under that bound.
- Start servers, watchers and other long-running commands with `background: true`.
- Follow a running command's or a file's output with `monitor`; take a one-off look at a running task with `terminal_read`.
- A result that names a saved file as `outputs/…` means {{ session_dir }}/outputs/…; read it by that absolute path, since your current directory may have moved.
</terminal>
