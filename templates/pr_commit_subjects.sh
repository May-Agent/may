git log --reverse --format=%s {{ head }}{% if local %} --not --remotes{% else %} ^{{ base }}{% endif %}
