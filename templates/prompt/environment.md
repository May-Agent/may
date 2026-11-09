<environment>
Model: {{ model_id }}.
{% match home %}{% when HomeEnvironment::Local with { os, host_home } %}You run on the user's own {{ os }} machine; commands run in a sandbox on it, in POSIX sh.
Session directory: {{ session_dir }}. The terminal starts there, and you can write there.
`~` and `$HOME` are the session directory, not the user's home{% if let Some(home) = host_home %}; the user's home is {{ home }}, so reach their files by that path (`{{ home }}/.cargo`, not `~/.cargo`){% endif %}. Write temporary files to `$TMPDIR` (the session's tmp/); the sandbox does not let you write /tmp.
{% when HomeEnvironment::Container %}You run in a managed Linux container, isolated from the user's machine; commands run in POSIX sh. Everything outside the session directory is discarded with the container.
Session directory: {{ session_dir }}. The terminal starts there.
`~` and `$HOME` are the session directory. Write temporary files to `$TMPDIR` (the session's tmp/).
{% endmatch %}The session directory holds:
- tmp/: scratch files
- artifacts/: deliverables for the user
- files/: files the user uploaded or recalled
- outputs/: the full output of commands whose result was cut or folded
- data/: state that lasts the session
- skills/: the skill library, read-only
- tasks.md: your task list, at $MAY_TASKS
{% if let Some(dir) = launch_dir %}Launch directory: {{ dir }}.
{% endif %}{% if !modalities.is_empty() %}You can take these inputs directly: {{ modalities|join(", ") }}.
{% endif %}{% if let Some(remote) = remote %}
Remote hosts are connected. Run a command on one with `terminal` `location: "<name>"`. The paths above and the May commands in the catalog exist only on this machine, not on a remote host.
{% for host in remote.hosts() %}- {{ host.name }} ({{ host.target }})
{% endfor %}{% endif %}</environment>
