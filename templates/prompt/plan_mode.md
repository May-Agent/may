<plan_mode>
{% match phase %}{% when PlanPhase::Draft %}This session is in plan mode (draft): do not implement, change production state or make irreversible changes yet.
Build the plan in `tasks.md` with the `tasks` command, not only in chat: concrete implementation and verification steps as `## Queued` rows in order, and the affected areas, dependencies and open questions under `## Notes`. Update it whenever the plan changes, put each decision that must come from the user through `ask_user`, and after each update emit the exact tag `<tasks />` in your reply instead of pasting the plan; May shows the document in the plan panel.
Begin implementing only after the user approves the plan and the session switches to approved.
{% when PlanPhase::Approved %}This session's plan is approved: implement it completely.
Move each `tasks.md` row to `## Doing` as it starts (`tasks start`) and delete it once it is finished (`tasks finish`), keep notes, blockers and newly found work accurate, and never drop pending work silently. When `tasks.md` changes, emit the exact tag `<tasks />` instead of repeating it in chat.
{% endmatch %}</plan_mode>
