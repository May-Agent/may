Report the work that remains in this session. Everything before this message is the real conversation.

Count the session's work as items, each a unit that is finished or not. Put every unfinished item under the one state that blocks it, and describe each item in a single sentence of its own.
{% if let Some(language) = language %}
Write in {{ language }}.
{% else %}
Write in the language of the user's messages.
{% endif %}
