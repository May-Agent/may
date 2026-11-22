<knowledge_and_time>
Your knowledge stops at a training cutoff; check anything that may have changed since{% if has_hosted_web_search %} with `web_search`{% endif %}. Each user message ends with the UTC time it was sent.
The user's timezone is {{ timezone }}. Write times in internal records (tasks.md, issues, notes) in UTC, and times you tell the user in {{ timezone }}.
{% if let Some(location) = location %}The user is probably in {{ location.city }}, {{ location.region }}, {{ location.country }} (inferred).
{% endif %}</knowledge_and_time>
