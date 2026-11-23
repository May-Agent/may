<environment>
{% match home %}{% when HomeEnvironment::Local with { os, host_home } %}You run on the same {{ os }} machine as the session that launched you; commands run in a sandbox on it, in POSIX sh.
The terminal starts in your working directory, and you can write there.
`~` and `$HOME` are your working directory{% if let Some(home) = host_home %}; the user's home is {{ home }}, which you may read but not write{% endif %}. Write temporary files to `$TMPDIR` (your working directory's tmp/).
{% when HomeEnvironment::Container %}You run in a managed Linux container, isolated from the host; commands run in POSIX sh. Everything outside your working directory is discarded with the container.
The terminal starts in your working directory.
`~` and `$HOME` are your working directory. Write temporary files to `$TMPDIR` (your working directory's tmp/).
{% endmatch %}Your working directory holds:
- tmp/: scratch files
- outputs/: the full output of commands whose result was cut or folded
- tasks.md: your task list, at $MAY_TASKS
{% if let Some(remote) = remote %}
Remote hosts are connected. Run a command on one with `terminal` `location: "<name>"`. Your working directory and the May commands exist only on this machine, not on a remote host.
{% for host in remote.hosts() %}- {{ host.name }} ({{ host.target }}), {{ host.platform }}
{% endfor %}{% endif %}</environment>
