# Context dump — `{{ session_id }}`

Captured `{{ captured_at }}` · model `{{ model_id }}` (provider `{{ provider_id }}`) · phase `{{ phase_label }}`

## Summary

- window: {{ metrics.context_window }} tokens — estimated usage {{ metrics.estimated_tokens }} (~{{ usage_pct }})
- stable prefix: {{ blocks.len() }} blocks — {{ prefix_chars }} chars (~{{ prefix_tokens }} tok)
- conversation messages: {{ metrics.recent_message_count }} ({{ metrics.recent_system_message_count }} system)
- handoff: {{ metrics.has_handoff }} · staged system updates pending: {{ metrics.has_staged_system_updates }}
- prompt cache epoch: {{ metrics.prompt_cache_epoch }}

## Stable system prefix — {{ blocks.len() }} blocks

Sent to the provider as a single `system` message: the blocks below joined with `\n`, in this order.

{% for block in blocks %}
### [{{ loop.index0 }}] `{{ block.tag }}` — {{ block.chars }} chars (~{{ block.tokens }} tok)

````
{{ block.content }}
````
{% if let Some(staged) = block.staged_update.as_deref() %}
Staged update, folds in at the next reassembly:

````
{{ staged }}
````
{% endif %}
{% endfor %}
## Wire messages — {{ messages.len() }}

The assembled request for the next turn, in order. `system prefix` = the blocks above joined · `handoff` = active handoff document · `conversation` = recent history and durable alerts · `live tail` = context rendered per request (tasks.md, background jobs, active skills).

{% for m in messages %}
### [{{ m.index }}] {{ m.role }} · {{ m.section.label() }} — {{ m.chars }} chars (~{{ m.tokens }} tok)
{% if let Some(id) = m.tool_call_id.as_deref() %}
tool_call_id: `{{ id }}`
{% endif %}
{%- for r in m.reasoning %}
reasoning: `{{ r.provider }}` — {{ r.payload_chars }} chars
{% endfor %}
{%- for call in m.tool_calls %}
tool call `{{ call.name }}` — {{ call.chars }} chars:
````
{{ call.arguments }}
````
{% endfor %}
{%- for att in m.attachments %}
attachment: {{ att }}
{% endfor %}
````
{{ m.content }}
````
{% endfor %}
