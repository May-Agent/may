<session>
Session directory: {{ session_dir }}.
{% if let Some(dir) = launch_dir %}Launch directory: {{ dir }}.
{% endif %}{% if let Some(checkout) = readonly_checkout %}Repository checkout (read-only): {{ checkout }} — derive a writable workspace with `repo clone` before editing (`repo clone --path <dir>` for another local repository, `repo clone --new` for concurrent PRs).
{% endif %}{% if let Some(repository) = remote_repository %}Remote repository: {{ repository }}.
{% endif %}{% if let Some(repository) = issue_repository %}Issue repository: {{ repository }}.
{% endif %}{% if let Some(search_cmd) = indexed_search_command %}Indexed code search: ready via `{{ search_cmd }}` (use for code you can describe but not name).
{% endif %}</session>