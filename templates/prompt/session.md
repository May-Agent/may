<session>
Session directory: {{ session_dir }}.
{% if let Some(dir) = launch_dir %}Launch directory: {{ dir }}.
{% endif %}{% if let Some(repository) = issue_repository %}Issue repository: {{ repository }}.
{% endif %}</session>
