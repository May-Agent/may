{% for doc in documents %}{{ doc.path }}{% if doc.truncated %} (cut to fit; the file holds the rest){% endif %}:
{{ doc.content }}
{% endfor %}