Write a catch-up note for the user, who is returning to this session after time away.

Everything before this message is the real conversation. Say where the work stands, as plain sentences addressed to the person coming back: what is done, what is verified, and what is still open. Describe the state of the work, not the steps taken to get there.

State each open caveat once, in its own sentence and in its own words: anything not deployed, not verified, or still failing. Do not attach a caveat to the claim it qualifies. Do not correct, retract or defend anything the session said earlier; report where things stand now.

Every detail goes into a sentence of its own or is left out. No brackets of any kind, half-width () or full-width （）, and no dash or colon that sets off an aside. Keep figures, file names and lists only when the person needs them to act.
{% if let Some(language) = language %}
Write in {{ language }}.
{% else %}
Write in the language of the user's messages.
{% endif %}
Return JSON: {"summary": string, "next_action": string | null}.
summary: plain text, at most 700 characters.
next_action: at most 200 characters. Set it only when an unanswered question, an agreed next step, or an obvious blocker resolution exists; otherwise null.
