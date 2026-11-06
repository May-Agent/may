{% for subject in subjects -%}
- {{ subject }}
{% endfor -%}
{% if let Some(issue) = github_issue %}
Closes #{{ issue }}
{% endif -%}
