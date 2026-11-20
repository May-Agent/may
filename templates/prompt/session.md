<session>
Session directory: {{ session_dir }}.
{% if let Some(dir) = launch_dir %}Launch directory: {{ dir }}.
{% endif %}{% if let Some(checkout) = readonly_checkout %}Repository checkout (read-only): {{ checkout }} — derive a writable workspace with `repo clone` before editing (`repo clone --new` for concurrent PRs).
{% endif %}{% if let Some(repository) = remote_repository %}Remote repository: {{ repository }}.
{% endif %}{% if let Some(repository) = issue_repository %}Issue repository: {{ repository }}.
{% endif %}</session>
