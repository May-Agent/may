Write a catch-up summary for a user returning to this session after time
away.

Everything before this message is the real conversation: the user's
requests and the assistant's replies. Summarize the state of the work, not
the process: concrete results — what is implemented, verified, or still
proposed. Preserve unresolved caveats above all else: anything not
deployed, not verified, or still failing must stay visible; that matters
more than identifiers, commit hashes, or durations.

Return JSON: {"summary": string, "next_action": string | null}.
summary: at most 700 characters, in the user's language, plain text,
40-80 words preferred.
next_action: at most 200 characters — set it only when an unanswered
question, an agreed next step, or an obvious blocker resolution exists;
otherwise null.
