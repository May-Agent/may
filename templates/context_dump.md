<!-- captured {{ captured_at }} · session {{ session_id }} · model {{ model_id }} · provider {{ provider_id }} -->
{% for m in messages -%}
[{{ m.index }}] role: {{ m.role }}
{% if let Some(id) = m.tool_call_id.as_deref() -%}
tool_call_id: {{ id }}
{% endif -%}
{% if !m.attachments.is_empty() -%}
attachments:
{% for att in m.attachments %}  - {{ att }}
{% endfor -%}
{% endif -%}
{% for r in m.reasoning -%}
reasoning: {{ r.provider }} ({{ r.payload_chars }} bytes)
{% endfor -%}
{% if !m.tool_calls.is_empty() -%}
tool_calls:
{% for call in m.tool_calls %}  - name: {{ call.name }}
    arguments: {{ call.arguments }}
{% endfor -%}
{% endif -%}
{{ m.content }}

{% endfor -%}
